//! The GPUI view that hosts a terminal: owns the backend, pumps PTY events into
//! redraws, translates keystrokes to bytes, and renders the terminal chrome.

use alacritty_terminal::event::Event as AlacEvent;
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Direction, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::TermMode;
use gpui::{
    App, ClipboardEntry, ClipboardItem, Context, ExternalPaths, FocusHandle, Focusable, Font,
    KeyDownEvent, Modifiers, MouseButton, MouseDownEvent, Pixels, ScrollDelta, ScrollWheelEvent,
    Window, actions, div, prelude::*, px,
};
use gpui_component::kbd::Kbd;
use gpui_component::menu::{ContextMenuExt, PopupMenuItem};
use gpui_component::{ActiveTheme as _, Icon, IconName, WindowExt as _, h_flex};

use super::TermSize;
use super::cmd_editor::CmdEditor;
use super::completion::{self, CandidateKind, CompletionSession};
use super::element::TerminalElement;
use super::highlight::{self, TokenKind};
use super::hold::{GapHold, Verdict};
use super::remote::RemoteTerminal;
use super::reverse_search::{self, ReverseSearch};
use super::search::{LinkTarget, SearchState};
use super::typeahead::{RawInput, Typeahead};
use crate::core::actions::{
    CloseActiveTab, ForkAgentSessionDown, ForkAgentSessionLeft, ForkAgentSessionRight,
    ForkAgentSessionUp, NewTab, SendBackTab, SendTab, SplitDown, SplitRight, ToggleMaximizePane,
};
use crate::core::config::{BellMode, Config, NotifyMode};
use crate::daemon::protocol::{RemoteContext, ShellSpec};

/// Inset (px) between the terminal-surface edge and the cell grid. The prompt
/// editor and the floating completion / history menus are absolutely positioned
/// over the grid, so they must offset their grid-aligned origin by the same
/// amount the surface padding insets the grid. Keep these in sync with the
/// `.px()/.py()` on the surface container in `TerminalView::render` — they are
/// the single source of truth for that inset.
const GRID_PAD_X: f32 = 8.;
const GRID_PAD_Y: f32 = 4.;

// Terminal-scoped actions dispatched by the right-click context menu and the
// menu bar's Edit menu. They route to this view via `.on_action` handlers on the
// terminal surface; tab/split actions in the same menu bubble up to `Tty7App`
// from the focused terminal.
//
// Every one of these is the *single* path for its gesture: the ⌘-chord, the
// context-menu row, and the Edit-menu item all dispatch the same action, so the
// three can't drift (they did — the context menu's Paste used to skip the
// image-paste branch that ⌘V had).
//
// `InsertNewline` is the exception to the "menu" part: it has no menu row (you
// do not reach for a menu mid-word), and exists so the prompt editor's soft
// newline is a *bindable* action rather than a hardcoded chord — see
// `insert_newline_action`.
actions!(
    terminal,
    [
        CopyText,
        CutText,
        PasteText,
        SelectAll,
        UndoEdit,
        RedoEdit,
        FindInTerminal,
        FindNext,
        FindPrevious,
        ClearScrollback,
        InsertNewline
    ]
);

/// Emitted when the pane's child process has genuinely exited (`exit`,
/// Ctrl-D, a crashed shell) — as opposed to the daemon connection dropping,
/// which keeps the dead pane visible. `Tty7App` subscribes (see
/// `new_terminal`) and closes the pane in response: collapsing its split, or
/// closing the tab when it was the only pane.
pub struct ChildExited;

impl gpui::EventEmitter<ChildExited> for TerminalView {}

/// A native-SSH pane raised an interactive auth/host-key prompt (or a status
/// change) that the app should surface in an in-pane sheet. `Tty7App` subscribes
/// (see `new_terminal`) and drains the pane's pending prompts into
/// `ui::ssh_prompt`. Zero-payload — the app reads the prompt off the pane's
/// `RemoteTerminal` (`take_auth_prompt` / `ssh_phase`).
pub struct AuthPromptReady;

impl gpui::EventEmitter<AuthPromptReady> for TerminalView {}

/// The pane's coding agent reported a different native session id than the one
/// the saved layout knows about — it started a conversation, or replaced the
/// one it had. `Tty7App` subscribes (see `new_terminal`) and re-saves.
///
/// # Why an event and not just "it's read at save time"
///
/// The id arrives asynchronously, on the agent's own hooks, long after
/// everything that *structurally* changes a window. Nothing else was making the
/// window save in between, so whether the id reached `session.json` came down
/// to whether the user happened to open a tab, split a pane or move focus
/// afterwards. That is what made resume-after-End-Sessions work sometimes and
/// not others: the layout on file simply had no agent in it.
pub struct AgentSessionChanged;

impl gpui::EventEmitter<AgentSessionChanged> for TerminalView {}

/// An established native-SSH daemon pane, ready to be wrapped in a view: the
/// output of the fallible [`TerminalView::spawn_native_ssh_terminal`], consumed
/// by the infallible [`TerminalView::from_native_ssh_parts`].
pub struct NativeSshParts {
    terminal: RemoteTerminal,
    pane_id: u64,
    /// Secret-free spec copy retained for session restore / in-pane reconnect.
    persist: Box<crate::daemon::protocol::NativeSshSpec>,
}

/// An established shell daemon pane (fresh spawn or re-attach), ready to be
/// wrapped in a view: the output of the fallible
/// [`TerminalView::spawn_shell_terminal_in`], consumed by the infallible
/// [`TerminalView::from_shell_parts`].
pub struct ShellParts {
    terminal: RemoteTerminal,
    /// The daemon's id for this pane. Readable so a pane that arrived after its
    /// slot was closed can be killed rather than leaked (see
    /// `ui::app::Tty7App::land_pane`).
    pub(crate) pane_id: u64,
    /// The explicit shell pick this pane was spawned with, if any; `None` for a
    /// re-attached pane (the pick isn't persisted).
    shell_spec: Option<ShellSpec>,
    /// The remote workspace this pane was opened *in*, carried through so
    /// [`TerminalView::from_shell_parts`] can bind the view to the same machine
    /// the connection went to. `None` for a local pane.
    ///
    /// It rides here rather than being set on the view afterwards because the
    /// two must not be able to disagree: the route is chosen before the pane
    /// exists, and a view that thought it was somewhere else would send its
    /// `Kill` and its restore `List` to the wrong daemon.
    ///
    /// Readable for the same reason as `pane_id` above: killing an orphaned
    /// pane has to dial the machine it actually landed on.
    pub(crate) workspace: Option<crate::terminal::PaneWorkspace>,
    /// Whether this is the pane the caller asked to re-attach to, or a fresh
    /// one spawned because that id was gone. A restored pane is still running
    /// whatever it was running; a respawned one is a bare shell in the same
    /// directory, which is the case a saved coding-agent session has to be
    /// resumed into.
    pub(crate) restored: bool,
    /// The workspace whose window created this pane — see
    /// [`TerminalView::owner_workspace`]. Rides here for the same
    /// cannot-disagree reason as `workspace` above.
    pub(crate) owner: Option<crate::core::session::WorkspaceId>,
}

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

/// Whether a pane's reported paths can be asked about on the host it is paired
/// with — the gate behind [`TerminalView::host_cwd`], lifted out so it can be
/// tested without standing up a pane and a daemon.
///
/// This is the one check keeping a remote path away from a `git` that cannot
/// see it. The pairing has to *agree*: a shell running on another machine is
/// only answerable by a host that is that machine, and a local shell only by
/// the local host. A pane that runs elsewhere but still holds the local host —
/// a native-SSH or WSL pane, which has no `Host` behind it at all — answers
/// `false`, which is exactly what the old `remote_context().is_none()` gate
/// did.
fn cwd_is_on_host(pane_runs_remotely: bool, host_is_local: bool) -> bool {
    match pane_runs_remotely {
        // A local shell: its paths are this machine's, so only the local host
        // can answer for them.
        false => host_is_local,
        // A shell on another machine: only a host that *is* that machine can.
        true => !host_is_local,
    }
}

pub struct TerminalView {
    pub terminal: RemoteTerminal,
    /// The machine whose filesystem and `git` this pane's paths belong to —
    /// [`HostId::LOCAL`] for a local or SSH pane, its workspace's machine for a
    /// remote-workspace pane (set by [`set_workspace`](Self::set_workspace)).
    ///
    /// **The id, not the host object.** A reconnect mints a fresh `RemoteHost`
    /// for the same machine and replaces the registry's entry; a pane holding
    /// the old `Arc` would keep probing a dead connection forever, and since a
    /// failed probe deliberately leaves the last good branch line on screen,
    /// that failure would look exactly like "nothing changed". Resolving
    /// through [`host`](Self::host) at use time means a reconnect is picked up
    /// by the next probe with nothing to notify.
    host_id: crate::ui::host_ops::HostId,
    /// The remote workspace this pane belongs to, when it is one.
    ///
    /// `None` — the case today, until the M5 window/workspace binding calls
    /// [`set_workspace`](Self::set_workspace) — means a local pane or an SSH
    /// pane, and every path that reads this falls back to the pane-addressed
    /// behaviour it has always had.
    ///
    /// It sits beside `host` rather than inside it because the two answer
    /// different questions: `host` is *whose filesystem is this path on*, this
    /// is *whose SSH connection do side effects run on, and what owns them*.
    /// Several workspaces share one `host`; they must not share forwards.
    workspace: Option<crate::terminal::PaneWorkspace>,
    /// Daemon-assigned id of the pane this view mirrors. Persisted in the session
    /// so a restart can re-`attach` to the still-running pane (process + scrollback
    /// intact) instead of spawning a fresh shell.
    pub pane_id: u64,
    /// The shell this pane was spawned with when the user picked one from the
    /// new-tab dropdown; `None` for the default shell and for re-attached
    /// panes. In-memory only (not persisted) — held so splits of this pane
    /// inherit the same shell.
    shell_spec: Option<ShellSpec>,
    /// The workspace whose window created this view (spawn or re-attach).
    /// `None` only for views built through paths that predate the field (tests,
    /// native SSH). `Tty7App::save_session` compares it against the workspace
    /// it is about to record under and shouts on a mismatch — a window whose
    /// tabs and identity have come apart is exactly the corruption that once
    /// copied one workspace's layout into another's record, and it must be
    /// caught at the write, not discovered at the next restart.
    owner_workspace: Option<crate::core::session::WorkspaceId>,
    /// Whether this view re-attached to the pane its caller asked for, rather
    /// than getting a fresh shell because that pane was gone — [`ShellParts`]'s
    /// `restored`, kept because restore has to act on it *after* the view is
    /// built.
    ///
    /// `false` for every view that was never restoring anything (a new tab, a
    /// split, a test), which is the same answer those callers already got from
    /// "there was no pane id to come back to".
    restored: bool,
    /// The native-SSH spec this pane was spawned with, **secrets stripped**
    /// ([`NativeSshSpec::without_secrets`]). `None` for local shells (and a
    /// foreground `ssh` typed in one). Persisted into the session so a *dead*
    /// native-SSH pane can be respawned/reconnected on restore (PRD FR-E4 / C2),
    /// and read live to drive the in-pane reconnect (`RestartSshSession`).
    ssh_spec: Option<Box<crate::daemon::protocol::NativeSshSpec>>,
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
    /// Last cell the pointer hovered over locally. Kept separate from
    /// `last_mouse_cell`, which belongs to terminal mouse-reporting protocol
    /// state and must not be disturbed by local link affordances.
    last_hover_cell: Option<(usize, usize)>,
    /// Whether the platform modifier (⌘ on macOS) is currently held, as reported
    /// by the window-level modifier listener. Mouse events can lag or omit this
    /// state while a mouse-tracking TUI is foreground, so link hover must not
    /// depend solely on each move event's modifier snapshot.
    link_modifier_down: bool,
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
    /// Force case-sensitive matching (the "Aa" toggle). When `false` the query
    /// keeps alacritty's smart-case default (insensitive unless it contains an
    /// uppercase char); when `true` a `(?-i)` prefix forces sensitivity. Persists
    /// across close/reopen of the bar.
    pub(super) search_case_sensitive: bool,
    /// Treat the query as a regex (the ".*" toggle). When `false` (default) the
    /// query is matched literally (metacharacters escaped); when `true` it is a
    /// regex pattern. Persists across close/reopen.
    pub(super) search_regex: bool,
    /// Set when the current query is regex mode and fails to compile — drives the
    /// error styling on the search field so an invalid pattern isn't a silent
    /// zero-match. Only ever true while `search_regex` is on.
    pub(super) search_regex_error: bool,
    /// The last query text, remembered when the bar closes so reopening restores
    /// it (unless a selection prefills instead).
    pub(super) search_last_query: String,
    /// True for a brief window after a bell event; drives a momentary visual
    /// flash painted in place of an audible beep.
    pub bell_flash: bool,
    /// Whether mouse events are reported to full-screen apps that request it
    /// (`Config::mouse_reporting`). Cached from the global at construction and
    /// refreshed on config hot-reload (`Tty7App::reload_from_config`) so the
    /// mouse-report gates — which run in `&self`/`&mut self` methods without a
    /// `cx` — can consult it. When `false`, every mouse-tracking mode reads as
    /// clear, keeping the mouse local (selection + scrollback).
    pub report_mouse: bool,
    /// Last observed "shell is idle at its prompt" state, tracked so a change can
    /// trigger a redraw (showing/hiding the line editor) even when the shell
    /// produced no output to repaint on its own.
    last_at_prompt: bool,
    /// When a foreground command is running, the instant it started and the tab
    /// title captured then — used to fire a "command finished" notification for
    /// long-running commands completed while the window is in the background.
    running_since: Option<std::time::Instant>,
    running_title: String,
    /// The coding agent (if any) detected during the current foreground-command
    /// episode, captured so its completion notification can be branded ("Claude
    /// Code finished" rather than a generic "command finished"). Set the moment
    /// the daemon reports an agent while a command runs; cleared when it ends.
    running_agent: Option<crate::core::cli_agent::CLIAgent>,
    /// The rich agent status last seen by the poll, so transitions (working →
    /// waiting, working → done) fire exactly one notification each and repaint
    /// the status dot.
    last_agent_status: Option<crate::core::cli_agent::AgentStatus>,
    /// The agent identity that is worth *persisting* — the native session id and
    /// the argv it was launched with — as last seen. Compared on every poll so a
    /// change raises [`AgentSessionChanged`] and the layout on file catches up.
    ///
    /// Deliberately not the agent chip itself: that can blip for a moment when
    /// the agent shells out, and a blip here would mean a save (and, on a remote
    /// workspace, a push) for nothing. The session id does not blip.
    last_agent_session: (Option<String>, Option<Vec<String>>),
    /// When the current rich turn entered `Working`, for the "finished after
    /// Ns" copy on its `Done` notification.
    agent_turn_started: Option<std::time::Instant>,
    /// Whether this pane's agent ever reported over the rich sentinel channel.
    /// While true, the coarse process-exit "agent finished" notification is
    /// suppressed — the turn-level `stop` events already said it better.
    agent_was_rich: bool,
    /// Whether the agent's last finished turn (the green `Done` dot) is *unread*
    /// — a turn ended that the user hasn't looked at since. Set when a new turn
    /// finishes while this pane is unfocused; cleared the moment the pane gains
    /// focus (you're looking at it). The tab avatar only paints the Done dot
    /// while this is true, so a result you've already seen stops nagging. Blue
    /// (working) / amber (waiting) are unaffected — they track live state.
    agent_result_unread: bool,
    /// One-shot guard for a manual "Mark as Unread" on the pane the dismissed
    /// context menu is about to refocus: closing the menu returns window focus
    /// to that pane, and the resulting focus-in would instantly clear the mark
    /// the user just made. Armed by [`mark_agent_result_unread`]
    /// (Self::mark_agent_result_unread); the next focus-in consumes it instead
    /// of clearing, so the mark survives until the user genuinely comes back.
    keep_unread_on_focus: bool,
    /// The cwd this pane's git line reads from (and last scheduled a probe
    /// for), so the poll loop only reprobes when the working directory
    /// actually changes. The snapshot itself lives in the process-wide
    /// [`GitStatusCache`](crate::terminal::git_status::GitStatusCache), keyed
    /// by work-tree root — panes in one repo share one entry instead of each
    /// computing (and staling) its own.
    git_status_cwd: Option<std::path::PathBuf>,
    /// The agent session's tool-completion count as of the last poll, so a
    /// change means "the agent ran a tool since we looked" — the cue to refresh
    /// the git line mid-turn rather than at the end of one. Reset to 0 when no
    /// session is present, so a new session's first tool call reads as activity.
    last_agent_activity: u64,
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
    /// Last-run metadata (timestamp + exit code) per history line, feeding the
    /// Ctrl+R menu's "ran 3h ago" and failure badges. Kept in step with
    /// `history` on submit; the exit code lands when the shell reports back.
    history_meta: std::collections::HashMap<String, super::history::EntryMeta>,
    /// `history` re-ordered by frecency (frequency × recency + a current-directory
    /// bonus), most relevant first. Drives the ghost-text autosuggestion — the
    /// sole whole-line recall surface besides Ctrl+R (the Tab menu stays
    /// history-free). Recomputed when a command is run or the working directory
    /// changes.
    history_ranked: Vec<String>,
    /// The frecency score of each `history` entry, index-aligned with it — the
    /// relevance half of the Ctrl+R search's fuzzy+frecency blend. Recomputed
    /// alongside `history_ranked`.
    history_frecency: Vec<f64>,
    /// The directory `history_ranked` was last computed for, so the polling loop
    /// only re-ranks when the working directory actually changes.
    ranked_cwd: Option<std::path::PathBuf>,
    /// Current position while navigating history with ↑/↓: `Some(i)` indexes
    /// `history`; `None` means we're editing a fresh line (past the newest entry).
    history_nav: Option<usize>,
    /// The in-progress line saved when history navigation starts, so pressing ↓
    /// past the newest entry restores what the user was typing.
    history_stash: String,
    /// Position of a run of ⌥. presses (readline's `yank-last-arg`): which
    /// `history` entry the last press took its word from, and the char span it
    /// left in the line — the next press replaces that span with the word from
    /// the entry before it. Any other key clears this, so the following ⌥.
    /// starts a fresh walk at the newest entry.
    last_word_nav: Option<LastWordWalk>,
    /// A submitted command whose history-file record is deferred until the
    /// shell reports back at its prompt, so the record can carry the command's
    /// exit code (see [`PendingHistory`]).
    pending_history: Option<PendingHistory>,
    /// Open Tab-completion menu, if any — a picker over the candidates gathered
    /// when it opened. Typing/Backspace re-filter it in place; it closes on
    /// accept, on Escape, or once the edited word no longer matches anything.
    completion: Option<CompletionSession>,
    /// Whether a remote directory listing for completion is on the wire (see
    /// [`Self::spawn_remote_path_completion`]). Holding Tab down would
    /// otherwise dial the daemon once per repeat while the first answer is
    /// still travelling.
    remote_completion_inflight: bool,
    /// Monotonic tag bumped every time a completion session opens or closes.
    /// Dynamic generators run on background threads and land their results here
    /// via `cx.spawn`; each task captures the generation it was spawned under and
    /// its result is dropped unless it still matches — so output from a session
    /// the user has since closed (or replaced) can never leak into a later menu.
    completion_generation: u64,
    /// While equal to the terminal's current `prompt_cycle`, the local line
    /// editor has handed this prompt's line over to the shell (Tab fell
    /// through to shell-native completion — see
    /// [`Self::handoff_tab_to_shell`]): the shell's own editor now holds the
    /// text, so keys go raw to the PTY exactly as on a shell-vi-mode prompt.
    /// Keyed to the entered-prompt *cycle*, not the raw report seq — a
    /// same-prompt redraw (completion list, `reset-prompt`) re-emits the
    /// PS1-embedded `133;B` and would bump the seq while zle still holds the
    /// handed-off text; re-engaging there would fork the two line buffers.
    /// Only a command actually running starts a new cycle and re-engages the
    /// editor.
    editor_handoff: Option<u64>,
    /// Active Ctrl+R history search, if any. While set, the editor shows a
    /// `(reverse-i-search)` prompt instead of the line and a menu of the ranked
    /// matches floats beside it: typing edits the query (fuzzy, blended with
    /// frecency), Ctrl+R/↓ and Ctrl+S/↑ move the selection, Enter accepts the
    /// selection into the line, Cmd+Enter runs it outright, and Escape/Ctrl+G
    /// cancels.
    reverse_search: Option<ReverseSearch>,
    /// One-shot "shell integration didn't engage" notice (#46). Set when Ctrl+R
    /// is pressed in a pane whose shell never reported OSC 133 — the history
    /// menu the user is reaching for can't appear, and without this the feature
    /// just looks broken. A figterm-style PTY shim (kiro-cli-term, qterm) that
    /// swallowed the reports is the usual culprit, so the message names the
    /// wrapper when the daemon's foreground query recognizes one. Cleared on
    /// the next keystroke, after a timeout, or if integration engages late.
    integration_notice: Option<String>,
    /// Latch so the notice shows at most once per pane — a diagnostic, not a nag.
    integration_notice_shown: bool,
    /// When this view was created. Ctrl+R inside the startup grace window stays
    /// silent: slow rc files mean integration legitimately hasn't reported yet.
    created_at: std::time::Instant,
    /// True while a left-drag that began on the command-editor line is in progress,
    /// so mouse-move extends the editor selection rather than the terminal's.
    editor_selecting: bool,
    /// True from a left press on the command-editor line until its release —
    /// unlike [`Self::editor_selecting`] it also covers the double/triple-click
    /// word/line selections, which don't arm drag-extend. Tells the mouse-up
    /// that the ended gesture selected in the editor, so copy-on-select copies
    /// the editor's selection rather than the terminal's.
    editor_select_gesture: bool,
    /// When a drag-extend is armed by a double-click, the word range the
    /// double-click selected, so the drag can grow the selection by whole words
    /// (keeping the anchor word intact). `None` for a plain char-granular drag.
    editor_drag_word: Option<(usize, usize)>,
    /// Sticky target column for vertical caret motion (↑/↓ across a multi-line
    /// buffer). Set on the first vertical step from the caret's current visual
    /// column and preserved across a run of ↑/↓ so passing through a short line
    /// doesn't lose the column; any other motion or edit clears it (`None`).
    editor_goal_col: Option<usize>,
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
/// endpoints are alacritty grid points (line = display row minus the scroll
/// offset), which stay fixed as the viewport scrolls. A link the terminal
/// wrapped — or a producer split across rows with a hard newline — spans
/// several rows, so `start` and `end` can sit on different lines.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct HoveredLink {
    pub start: Point,
    pub end: Point,
}

enum LoopbackOpen {
    Forwarded(String),
    ForwardFailed(String),
    NotLoopback,
}

/// How a ⌘/Ctrl-clicked URL should be opened, decided before anything is done
/// about it.
///
/// Split out as a pure decision so the branch a pane takes is testable without a
/// daemon, a connection, or a browser — the three things this feature otherwise
/// needs all at once.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum LoopbackPlan {
    /// Not a loopback URL, forwarding is off, or the pane's shell runs on this
    /// machine: hand the URL to the OS unchanged.
    Direct,
    /// A remote whose `localhost` *is* the client's — WSL shares the network
    /// namespace with its Windows host (the exception). No forward is
    /// built; the original URL already resolves. Wired now so M8 only has to
    /// start constructing `RemoteTarget::Wsl`.
    NoForwardNeeded,
    /// A native-SSH pane ("连一下"): forward on the pane's own connection, owned
    /// by the pane, torn down with it.
    ForwardOnPane(u64),
    /// A remote-workspace pane ("在上面开发"): forward on the workspace's
    /// connection, owned by the workspace so it outlives the pane.
    ForwardOnWorkspace(Box<crate::terminal::PaneWorkspace>),
}

/// The ⌘-click routing rule, as a pure function of what the pane is.
///
/// The workspace is checked *before* the pane's own remote context, and that
/// order is the whole point: a remote workspace's panes are ordinary local
/// shells as far as the remote daemon is concerned, so they carry no
/// `RemoteContext` at all and the SSH-pane test below would decline them.
pub(super) fn loopback_plan(
    enabled: bool,
    workspace: Option<&crate::terminal::PaneWorkspace>,
    remote_kind: Option<crate::daemon::protocol::RemoteKind>,
    pane_id: u64,
) -> LoopbackPlan {
    // The user's off switch wins everywhere, including hover detection — a
    // `localhost:3000` that will not be forwarded must not underline either.
    if !enabled {
        return LoopbackPlan::Direct;
    }
    if let Some(ws) = workspace {
        if ws.shares_localhost() {
            return LoopbackPlan::NoForwardNeeded;
        }
        // A non-WSL workspace with nothing to forward over cannot be reached;
        // opening the client's own `localhost` would be a wrong answer dressed
        // up as a right one, so decline and let the URL through untouched.
        if ws.spec.is_none() {
            log::warn!("remote workspace has no connection spec; not forwarding localhost links");
            return LoopbackPlan::Direct;
        }
        return LoopbackPlan::ForwardOnWorkspace(Box::new(ws.clone()));
    }
    // A native-SSH pane forwards over its own russh connection (FR-F4).
    match remote_kind {
        Some(crate::daemon::protocol::RemoteKind::NativeSsh) => {
            LoopbackPlan::ForwardOnPane(pane_id)
        }
        _ => LoopbackPlan::Direct,
    }
}

/// A submitted command whose history-file record is deferred so it can carry
/// the command's exit code (like zsh's `INC_APPEND_HISTORY_TIME`). `seq` is
/// [`RemoteTerminal::prompt_seq`] at submit time: a later report that puts the
/// shell back at its prompt means the run completed and `last_exit_code()` is
/// this command's. Flushed without an exit code if the view goes away first.
struct PendingHistory {
    line: String,
    cwd: Option<std::path::PathBuf>,
    ts: u64,
    seq: u64,
}

/// Where a run of ⌥. presses has walked to (see
/// [`TerminalView::last_word_nav`]).
struct LastWordWalk {
    /// Index into `history` the last press took its word from.
    entry: usize,
    /// Char offset of the word it inserted — the next press swaps that span
    /// for the word from an older entry.
    at: usize,
    /// The word itself: both the span's length and a fingerprint. Edits that
    /// bypass `handle_editor_key` (IME-committed text, a paste, a completion
    /// pick, ⌘Z) can't clear `last_word_nav`, so before resuming, the walk
    /// checks the line still holds this word at `at` with the caret at its
    /// end — anything else means an edit intervened and the walk starts over
    /// rather than eating it.
    word: String,
}

/// Seconds since the unix epoch — the timestamp history records carry.
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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

// The minimum foreground-command duration worth a "finished" notification is
// configurable (`Config::notify_threshold_secs`, default 10s); read live where
// the notification is posted rather than pinned to a const here.

/// How long gap input may be held client-side before it must be released to
/// the PTY (see the `hold` module). Long enough for a fast command's full
/// round trip (`133;D` report back to this client — tens of ms), short enough
/// that typing into a program that reads stdin right after launch feels
/// instant once the window lapses.
const HOLD_WINDOW: std::time::Duration = std::time::Duration::from_millis(150);

/// How long after pane creation Ctrl+R stays silent about missing shell
/// integration: slow rc files can take several seconds to reach the first
/// prompt report, and calling integration broken while the shell is still
/// starting up would be a false alarm.
const INTEGRATION_GRACE: std::time::Duration = std::time::Duration::from_secs(8);

/// How long the integration notice stays up when no keystroke dismisses it.
const INTEGRATION_NOTICE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Floor on how often an *opportunistic* git probe may run for one cwd (see
/// [`GitRefresh::Opportunistic`]). Short enough that the sidebar's counts feel
/// live while an agent works, long enough that a burst of tool calls — or an
/// alt-tab into a window holding a dozen panes — collapses into one `git`
/// shell-out per repo instead of a dozen.
const OPPORTUNISTIC_GIT_GAP: std::time::Duration = std::time::Duration::from_millis(1500);

/// Why a git-status probe is being asked for — the two classes get opposite
/// treatment when one is already in flight.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum GitRefresh {
    /// A rare state change that must not be missed: the pane changed
    /// directory, a command finished, an agent turn ended. Queues behind an
    /// in-flight probe (which then reruns) rather than being dropped.
    Edge,
    /// A cheap signal that repeats on its own: the window regained focus, the
    /// agent finished a tool call. Dropped outright when a probe is in flight
    /// or one ran within [`OPPORTUNISTIC_GIT_GAP`] — the next one will come.
    Opportunistic,
}

/// Fig-descended PTY shims known to exec over the shell we spawned and re-host
/// it on a nested PTY without forwarding OSC 133 — which starves shell
/// integration and silently kills the whole command-editor overlay (#46).
/// Matched against the foreground process name the daemon reports; `contains`
/// because the shim may present as e.g. `zsh (kiro-cli-term)`.
fn known_pty_shim(fg: &str) -> Option<&'static str> {
    ["kiro-cli-term", "figterm", "qterm", "cwterm"]
        .into_iter()
        .find(|shim| fg.contains(shim))
}

/// The integration-notice text. Naming the shim matters: "install shell
/// integration" advice would mislead — the hooks *are* installed, something
/// between the shell and tty7 is eating their reports.
fn integration_notice_message(wrapper: Option<&str>) -> String {
    match wrapper {
        Some(w) => format!(
            "tty7 shell integration is blocked in this pane — \u{201c}{w}\u{201d} is intercepting \
             shell reports, so inline completion and the Ctrl+R menu are unavailable. \
             The shell's own history search still works."
        ),
        None => "tty7 shell integration hasn't engaged in this pane, so inline completion and \
                 the Ctrl+R menu are unavailable. A PTY wrapper (figterm-style) or an \
                 unsupported shell setup can cause this."
            .to_string(),
    }
}

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

/// Post a branded "the agent finished" notification — the coding-agent form of
/// [`notify_command_finished`], titled with the agent so it's obvious *which*
/// session came back.
fn notify_agent_finished(agent: crate::core::cli_agent::CLIAgent, elapsed: std::time::Duration) {
    let secs = elapsed.as_secs();
    let body = format!("Finished after {secs}s");
    super::remote::notify_desktop(Some(agent.display_name()), &body);
}

/// Ring the OS system bell for the `Audible` bell mode. Returns `true` if a
/// sound was actually requested, `false` on platforms without a portable beep
/// (the caller then falls back to the visual flash so the bell is never silent).
fn ring_system_bell() -> bool {
    #[cfg(target_os = "macos")]
    {
        // A parameter-less AppKit call that just asks the system to play the
        // user's alert sound; invoked on the main (gpui app) thread, where every
        // `AlacEvent` is handled.
        objc2_app_kit::NSBeep();
        true
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
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

/// Build the byte sequence that submits the local editor's buffer to the shell.
///
/// The line count is what matters here. Replaying every embedded newline as its
/// own CR makes the shell's line editor run a *full* prompt cycle per line —
/// preexec, the user's precmd chain (git-status prompts, conda…), a
/// syntax-highlight pass over the whole buffer, plus our own OSC 133 `D`
/// follow-up work. A 30-line paste costs 30 of them and visibly crawls down the
/// screen, as if the command were being retyped. Under bracketed paste the
/// whole buffer goes in as a single paste and one CR accepts it: one prompt
/// cycle whatever the line count.
///
/// Continuation still works — better, in fact. zle keeps the embedded newlines
/// in its buffer, so a backslash / open-quote / heredoc command parses as one
/// unit; the PS2 assembly the per-line replay existed for now happens inside
/// the buffer instead of on the wire. The visible difference is that the block
/// executes as one unit and lands in the shell's history as one entry — which
/// is what pasting multi-line text into any other terminal already does, and
/// what our own history has always recorded.
///
/// ESC is stripped first (in both branches, unlike the paste path): clipboard
/// text carrying its own `ESC[201~` could otherwise close the paste early and
/// have the rest run as typed input, and a raw ESC reaching zle unbracketed is
/// an editor command, not text. CR is normalized away in the same pass, so a
/// CRLF clipboard is one line break either way rather than a stray blank Enter.
///
/// An empty buffer skips the markers: zsh's `bracketed-paste-magic` (which
/// oh-my-zsh turns on) errors on a paste with nothing between them.
///
/// The agent-prompt path already delivers multi-line text this way — see
/// [`crate::core::agent_prompt::submit_bytes`], which is this shape minus the
/// unbracketed fallback (an agent TUI always enables the mode).
fn submit_bytes(line: &str, bracketed: bool) -> Vec<u8> {
    // A CRLF clipboard pastes into the editor verbatim, so the `\r` has to go
    // before either branch sees it. Unbracketed it would be a second Enter
    // (`\r\n` → `\r\r`, a blank line submitted mid-command); bracketed it would
    // ride inside the markers and land on whatever the far side happens to do
    // with a CR in a paste — zsh turns it into a newline, so the block gains a
    // blank line, and a shell that doesn't leaves a literal `^M` in the command.
    // One `\n` per line is the shape both branches are written for.
    let clean: String = line
        .replace("\r\n", "\n")
        .chars()
        .filter(|&c| c != '\x1b')
        .map(|c| if c == '\r' { '\n' } else { c })
        .collect();
    let mut bytes = paste_bytes(&clean, bracketed && !clean.is_empty());
    bytes.push(b'\r');
    bytes
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
/// single argument instead of splitting. Mirrors how macOS Terminal.app turns
/// a dropped/pasted file into command-line text. An empty path
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
/// space-safe path (multiple files → space-separated args), matching macOS
/// Terminal.app. gpui's own `ClipboardItem::text()` would instead
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

/// Stage a clipboard image as a temp file so [`paste_clipboard_image`] can paste
/// its path. Web-friendly formats a coding agent's vision accepts (PNG/JPEG/GIF/
/// WebP) are written through untouched; anything else — notably the BMP that
/// Windows screenshots (`CF_DIB`) arrive as — is transcoded to PNG, since agent
/// vision rejects those. Returns the path, or `None` if decoding/writing failed.
///
/// The filename is keyed on gpui's content hash of the bytes, so re-pasting the
/// same screenshot reuses one file instead of accumulating temp copies (this
/// crate has no `Date`/random to mint a unique name with anyway).
#[cfg(not(target_os = "macos"))]
fn write_clipboard_image(img: &gpui::Image) -> Option<std::path::PathBuf> {
    use gpui::ImageFormat;
    let dir = std::env::temp_dir().join("tty7-clipboard");
    std::fs::create_dir_all(&dir).ok()?;
    let (ext, transcoded) = match img.format {
        ImageFormat::Png => ("png", None),
        ImageFormat::Jpeg => ("jpg", None),
        ImageFormat::Gif => ("gif", None),
        ImageFormat::Webp => ("webp", None),
        other => ("png", Some(transcode_to_png(&img.bytes, other)?)),
    };
    let data: &[u8] = transcoded.as_deref().unwrap_or(&img.bytes);
    let path = dir.join(format!("paste-{:016x}.{ext}", img.id));
    std::fs::write(&path, data).ok()?;
    Some(path)
}

/// Decode `bytes` (in `format`) and re-encode as PNG. SVG can't be rasterized by
/// the `image` crate, so it — and any decode/encode failure — yields `None`.
#[cfg(not(target_os = "macos"))]
fn transcode_to_png(bytes: &[u8], format: gpui::ImageFormat) -> Option<Vec<u8>> {
    use gpui::ImageFormat as G;
    let src = match format {
        G::Png => image::ImageFormat::Png,
        G::Jpeg => image::ImageFormat::Jpeg,
        G::Webp => image::ImageFormat::WebP,
        G::Gif => image::ImageFormat::Gif,
        G::Bmp => image::ImageFormat::Bmp,
        G::Tiff => image::ImageFormat::Tiff,
        G::Ico => image::ImageFormat::Ico,
        G::Pnm => image::ImageFormat::Pnm,
        G::Svg => return None,
    };
    let decoded = image::load_from_memory_with_format(bytes, src).ok()?;
    let mut out = Vec::new();
    decoded
        .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .ok()?;
    Some(out)
}

/// The font fallback chain: the user's configured list, then this platform's
/// stock faces, then the bundled "Hack" pinned to the end.
///
/// Hack ships inside the binary (`register_bundled_fonts`) and covers the
/// symbols prompt themes lean on — `❯`, `➜`, box drawing, the sharp powerline
/// wedges — with ink that fits a monospace advance. Without this anchor, a
/// custom `font_family` that lacks one of those codepoints falls through the
/// whole configured list into the OS cascade, which happily serves a
/// proportional glyph wider than the cell that `paint_glyphs`' per-cell clip
/// then truncates (issue #17's severed `➜`).
///
/// Hack carries no CJK at all (1548 codepoints mapped, zero ideographs), so on
/// a chain that names only macOS faces every Chinese character falls through to
/// the OS cascade too. [`platform_last_resort_fallbacks`] is appended for the
/// same reason the Hack anchor exists — to keep the last word ours rather than
/// the cascade's — and it repairs already-persisted configs, which a change to
/// `Config::default` alone would never reach.
fn fallback_chain(family: &str, configured: &[String]) -> Vec<String> {
    let mut chain = configured.to_vec();
    let mut pin = |name: &str| {
        if family != name && !chain.iter().any(|f| f == name) {
            chain.push(name.to_string());
        }
    };
    for name in crate::core::config::platform_last_resort_fallbacks() {
        pin(name);
    }
    pin("Hack");
    chain
}

impl TerminalView {
    /// The fallible half of a shell-backed view: establish the daemon pane
    /// *before* the view is built, so a refused spawn (daemon down, spawn
    /// error) comes back as an `Err` the caller can report. Splitting it out
    /// matters beyond tidiness: the view is constructed inside `cx.new`, deep
    /// under gpui's `extern "C"` input callbacks, where a panic can't unwind
    /// and aborts the process instead. Mirrors
    /// [`Self::spawn_native_ssh_terminal`].
    ///
    /// Provisional size; corrected on the first prepaint once we can measure.
    /// The PTY lives in the daemon now. On session restore (`restore_pane`),
    /// re-`attach` to the still-running pane so its process + scrollback come
    /// back intact; otherwise `spawn` a fresh pane (with the caller's shell
    /// pick, if any).
    ///
    /// **A `restore_pane` that is gone falls back to a fresh pane** rather than
    /// failing. Callers do check first, but neither check is a guarantee: a
    /// local one asks the daemon and can be raced by the pane exiting, and a
    /// remote one cannot afford to ask at all (`alive_panes_on` is a blocking
    /// round trip and the UI thread is where it would run) — trying the attach
    /// *is* the question there. Either way an id that no longer exists is the
    /// ordinary state of a workspace whose sessions were ended, and the answer
    /// to it is the same as for a session written before the daemon existed: a
    /// fresh shell in the saved cwd. `restored` says which happened, because
    /// what the caller does next differs — see [`ShellParts`].
    ///
    /// **`workspace: None` is the local path, unchanged down to the byte** —
    /// [`PaneRoute::for_workspace`] answers `Local`, and `Local` is a bare
    /// `transport::connect()` with no header in front of it. There is no
    /// "remote-aware" branch a local pane passes through.
    ///
    /// `Some(_)` is the whole of what makes a remote pane remote: the daemon is
    /// told which machine the connection is for before any `ClientMsg` goes out,
    /// and everything after — `Spawn`, `Attach`, `Input`, `Output` — lands on
    /// that machine's `tty7-server` instead of this one's daemon.
    pub fn spawn_shell_terminal_in(
        workspace: Option<crate::terminal::PaneWorkspace>,
        working_directory: Option<std::path::PathBuf>,
        restore_pane: Option<u64>,
        shell: Option<ShellSpec>,
        owner: Option<crate::core::session::WorkspaceId>,
    ) -> anyhow::Result<ShellParts> {
        let route = crate::terminal::PaneRoute::for_workspace(workspace.as_ref());
        let attached = match restore_pane {
            Some(id) => match RemoteTerminal::attach_on(&route, TermSize::new(80, 24), 8, 17, id) {
                // An attached pane keeps whatever shell it already runs; the
                // pick that spawned it (if any) isn't persisted.
                Ok(terminal) => Some((terminal, id, None)),
                Err(e) => {
                    log::info!("pane {id} is gone on its machine ({e:#}); spawning fresh");
                    None
                }
            },
            None => None,
        };
        let restored = attached.is_some();
        let (terminal, pane_id, shell_spec) = match attached {
            Some(parts) => parts,
            None => {
                let (terminal, id) = RemoteTerminal::spawn_on(
                    &route,
                    TermSize::new(80, 24),
                    8,
                    17,
                    working_directory,
                    shell.clone(),
                    owner.map(|id| id.to_string()),
                )?;
                (terminal, id, shell)
            }
        };
        Ok(ShellParts {
            terminal,
            pane_id,
            shell_spec,
            workspace,
            restored,
            owner,
        })
    }

    /// Wrap an established shell pane (from [`Self::spawn_shell_terminal_in`]) in
    /// a view. Infallible by construction — see that function.
    pub fn from_shell_parts(
        parts: ShellParts,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self::with_terminal(parts.terminal, parts.pane_id, window, cx);
        view.shell_spec = parts.shell_spec;
        view.owner_workspace = parts.owner;
        view.restored = parts.restored;
        view.set_workspace(parts.workspace);
        view
    }

    /// Whether this pane came back as the one it was asked to re-attach to.
    /// `false` means a fresh shell — see the field, and
    /// [`ShellParts::restored`].
    pub(crate) fn restored(&self) -> bool {
        self.restored
    }

    /// The workspace whose window created this pane, or `None` when the
    /// creating path predates the field. See the field for what reads it.
    pub fn owner_workspace(&self) -> Option<crate::core::session::WorkspaceId> {
        self.owner_workspace
    }

    /// Spawn a native (russh) SSH pane for `spec` and build the view around it
    /// (PRD FR-C1/E-series). The caller (`ui::ssh_connect`) has already resolved
    /// keychain secrets into `spec`; this view retains only the **secret-free**
    /// copy ([`NativeSshSpec::without_secrets`]) for session-restore respawn and
    /// the in-pane reconnect. Auth/host-key prompts and the connection phase ride
    /// this pane's own stream and surface through the usual `AuthPromptReady`
    /// path.
    /// The fallible half of a native-SSH view: establish the daemon pane first,
    /// so a refused spawn (daemon down, stale pre-SSH daemon, protocol error)
    /// surfaces as an `Err` the caller can report — building the view itself
    /// (inside `cx.new`, via [`Self::from_native_ssh_parts`]) has no failure
    /// path of its own.
    pub fn spawn_native_ssh_terminal(
        spec: Box<crate::daemon::protocol::NativeSshSpec>,
        working_directory: Option<std::path::PathBuf>,
    ) -> anyhow::Result<NativeSshParts> {
        let persist = Box::new(spec.without_secrets());
        let (terminal, pane_id) = RemoteTerminal::spawn_native_ssh(
            TermSize::new(80, 24),
            8,
            17,
            working_directory,
            spec,
        )?;
        Ok(NativeSshParts {
            terminal,
            pane_id,
            persist,
        })
    }

    /// Wrap an established native-SSH pane (from
    /// [`Self::spawn_native_ssh_terminal`]) in a view.
    pub fn from_native_ssh_parts(
        parts: NativeSshParts,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self::with_terminal(parts.terminal, parts.pane_id, window, cx);
        view.ssh_spec = Some(parts.persist);
        view
    }

    /// Build the view around an already-connected terminal. Split from [`new`]
    /// so tests can hand in a `RemoteTerminal` backed by a plain socketpair
    /// and exercise the event plumbing without a live daemon — see
    /// [`quiet_test_pane`], which the UI-level tests build their tabs from.
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
        let font_features = config
            .font_features
            .as_ref()
            .map(crate::core::config::gpui_font_features);
        let report_mouse = config.mouse_reporting;
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
                // Looking at the pane marks its finished turn read, so the tab
                // avatar's green Done dot clears — unless this focus-in is
                // just the context menu handing focus back after a manual
                // "Mark as Unread" (the one-shot guard eats it).
                if view.keep_unread_on_focus {
                    view.keep_unread_on_focus = false;
                } else {
                    view.agent_result_unread = false;
                }
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
        let history_frecency =
            super::history::frecency_scores(&history.entries, &history.counts, &history.cwds, None);

        Self {
            terminal,
            host_id: crate::ui::host_ops::HostId::LOCAL,
            workspace: None,
            pane_id,
            shell_spec: None,
            owner_workspace: None,
            restored: false,
            ssh_spec: None,
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
            report_mouse,
            last_hover_cell: None,
            link_modifier_down: false,
            scroll_debt: 0.,
            scroll_frac: 0.,
            search: None,
            cursor_visible: true,
            focused: true,
            search_focused: false,
            search_case_sensitive: false,
            search_regex: false,
            search_regex_error: false,
            search_last_query: String::new(),
            bell_flash: false,
            last_at_prompt: false,
            running_since: None,
            running_title: String::new(),
            running_agent: None,
            last_agent_status: None,
            // Empty rather than seeded from the saved layout: a pane that comes
            // back attached to a running agent then reports the id it already
            // had, which reads as a change and saves once. Harmless, and the
            // alternative — trusting the record — would skip the save that
            // fixes a record which is *wrong*.
            last_agent_session: (None, None),
            agent_turn_started: None,
            agent_was_rich: false,
            agent_result_unread: false,
            keep_unread_on_focus: false,
            git_status_cwd: None,
            last_agent_activity: 0,
            cmd: CmdEditor::new(),
            typeahead: Typeahead::new(),
            hold: GapHold::new(),
            history: history.entries,
            history_counts: history.counts,
            history_cwds: history.cwds,
            history_meta: history.meta,
            history_ranked,
            history_frecency,
            ranked_cwd: None,
            history_nav: None,
            history_stash: String::new(),
            last_word_nav: None,
            pending_history: None,
            completion: None,
            completion_generation: 0,
            editor_handoff: None,
            remote_completion_inflight: false,
            reverse_search: None,
            integration_notice: None,
            integration_notice_shown: false,
            created_at: std::time::Instant::now(),
            editor_selecting: false,
            editor_select_gesture: false,
            editor_drag_word: None,
            editor_goal_col: None,
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
        // A remembered hover cell describes the *old* geometry: after a resize
        // its row may not exist any more, and the pointer sits over a different
        // cell regardless. Forget it — the next mouse move records a fresh one.
        // (`grid_line` also refuses a stale row, so this is about not underlining
        // the wrong cell, not about safety.) The link that cell resolved to goes
        // with it: it is held in grid coordinates the reflow just moved text
        // under, so keeping it would underline whatever now sits there (and hold
        // the pointing-hand cursor over it) until the pointer moves again.
        if (cols, rows) != (self.terminal.size().cols, self.terminal.size().rows) {
            self.last_hover_cell = None;
            self.hovered_link = None;
        }
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

    pub fn remote_context(&self) -> Option<RemoteContext> {
        self.terminal.remote_context()
    }

    /// The pane's cwd *only when it names a directory on this machine* — the
    /// accessor every local filesystem or `Command` use must go through.
    ///
    /// A remote pane's OSC 7 reports a path in the remote's namespace
    /// (`/home/me/proj` from an SSH host). Feeding that to a local `git` or
    /// `read_dir` is meaningless, and on Windows it is worse than meaningless:
    /// `/home/me/proj` is not an absolute path there but a *drive-relative*
    /// one, so it silently resolves to `C:\home\me\proj`. That usually just
    /// fails an `exists()` check — but if such a directory happens to exist,
    /// the pane reports an unrelated local repo's branch and diff as its own.
    /// Correctness must not rest on that collision never happening.
    ///
    /// Note this gates on the pane being remote, not on the shape of the path:
    /// a local shell may legitimately sit in a directory whose name looks
    /// remote, and Git Bash reports genuinely local paths (via `pwd -W`) that
    /// merely originate from a POSIX-looking shell.
    pub fn local_cwd(&self) -> Option<std::path::PathBuf> {
        self.paths_are_local().then(|| self.cwd())?
    }

    /// Whether this pane's paths name files on *this* machine.
    ///
    /// Two independent ways they may not, and a caller that checks one and not
    /// the other is silently wrong:
    ///   - **`remote_context`** — the pane's *own* process is elsewhere (a pane
    ///     tty7 dialled over SSH, a `wsl.exe` pane, a foreground `ssh`). The
    ///     daemon reports it, having watched the process.
    ///   - **`host_id`** — the pane belongs to a **remote workspace**.
    ///     Nothing about the pane itself is remote *from its own daemon's point
    ///     of view*: `tty7-server` on the far machine spawned an ordinary local
    ///     shell and reports `remote_context: None`, exactly as a local daemon
    ///     would. It is the daemon that is on another machine, which no
    ///     pane-level signal can express — only this side's binding knows.
    ///
    /// Which is why the second test cannot be folded into the first, and why
    /// every use goes through here rather than re-deriving it: a routed pane
    /// answering "yes, local" hands its remote cwd to `read_dir`, to `git`, and
    /// to the file opener, all of which then answer about the wrong machine.
    fn paths_are_local(&self) -> bool {
        self.remote_context().is_none() && self.host_id.is_local()
    }

    /// The pane's cwd *as the machine a sibling pane will spawn on reads it* —
    /// what "+", a split, and the persisted session hand the new shell.
    ///
    /// Deliberately **not** [`local_cwd`](Self::local_cwd), and the difference
    /// is the whole point. A window shows one machine, so a sibling lands
    /// on the machine this pane's shell already runs on: for a remote-workspace
    /// pane that is the far box, where `/home/me/proj` is exactly right and
    /// withholding it would open every new tab at `$HOME` instead.
    ///
    /// What still has to decline is a pane whose shell is on a machine the
    /// sibling will *not* be on — a native-SSH or WSL pane, whose window is
    /// otherwise local. `remote_context` is precisely that condition, and it is
    /// why these callers cannot share the strict accessor: they ask "will the
    /// new shell be able to chdir here", not "is this file on my disk".
    pub fn spawnable_cwd(&self) -> Option<std::path::PathBuf> {
        self.remote_context().is_none().then(|| self.cwd())?
    }

    /// The machine this pane's paths live on — what every filesystem or `git`
    /// question about this pane must be asked of.
    ///
    /// `None` means that machine is not around: a remote workspace whose
    /// connection closed, or one this process has not connected to yet. It is
    /// never a local pane — the local host is always resolvable. Callers stop
    /// there rather than falling back to this machine; asking the local git
    /// about a remote path is how a pane ends up showing *another* repository's
    /// branch, which is the bug this whole indirection exists to prevent.
    pub fn host(&self, cx: &gpui::App) -> Option<crate::ui::host_ops::SharedHost> {
        crate::ui::host_registry::HostRegistry::lookup(cx, self.host_id)
    }

    /// The id alone — for the cache lookups and comparisons that never need the
    /// host object, and so keep working while a machine is disconnected.
    pub fn host_id(&self) -> crate::ui::host_ops::HostId {
        self.host_id
    }

    /// The remote workspace this pane belongs to, if any — what its port
    /// forwards are owned by and whose SSH connection its SFTP rides.
    pub fn workspace(&self) -> Option<&crate::terminal::PaneWorkspace> {
        self.workspace.as_ref()
    }

    /// Bind this pane to a remote workspace.
    ///
    /// A setter rather than a constructor argument so the one
    /// `TerminalView::new` keeps the shape every existing call site already
    /// passes, and a local pane needs no change at all.
    ///
    /// Called by [`from_shell_parts`](Self::from_shell_parts) with the workspace
    /// the pane's *connection* was routed to, so the two cannot drift apart.
    /// Calling it with anything else re-labels a pane without moving it, which
    /// is why nothing else does.
    ///
    /// **This is also what points the pane's path questions at the right
    /// machine.** The host id comes off the workspace's own `RemoteTarget`,
    /// through the same `connection_key` the connection was opened under — so
    /// the id resolves to the very host object
    /// [`RemoteConnections::insert`](crate::ui::remote_connect::RemoteConnections::insert)
    /// registered, with no second source of truth to drift from it. Setting the
    /// route and setting the host is one operation because a pane that ran its
    /// shell on one machine and its `git` on another would be worse than
    /// either.
    pub fn set_workspace(&mut self, workspace: Option<crate::terminal::PaneWorkspace>) {
        self.host_id = workspace
            .as_ref()
            .map_or(crate::ui::host_ops::HostId::LOCAL, |w| w.target.host_id());
        self.workspace = workspace;
    }

    /// Where this pane's daemon connections go.
    ///
    /// Anything addressed by `pane_id` — `Kill`, the restore-time `List` — has to
    /// use this rather than the plain local call, because pane ids are per-daemon
    /// and a remote pane's id names a *different* daemon's pane. Returns
    /// [`PaneRoute::Local`] for a local pane, which is the call every one of
    /// those sites makes today.
    pub fn pane_route(&self) -> crate::terminal::PaneRoute {
        crate::terminal::PaneRoute::for_workspace(self.workspace.as_ref())
    }

    /// The read-only degrade, as the keyboard sees it.
    ///
    /// **A local pane always answers `true`** — it has no connection to lose,
    /// and `workspace()` is `None` for it, so this is a field test and not a
    /// lookup. A remote pane defers to its workspace's connection state, which
    /// is the workspace's business and not a pane's: five entry points ask, one
    /// rule answers.
    ///
    /// Deliberately **not** consulted by
    /// [`handle_event`](Self::handle_event)'s `PtyWrite` arm. Those bytes are
    /// the emulator answering a question the *remote program* asked — a DA
    /// report, an OSC colour reply, a cursor-position report — and gating them
    /// would leave that program waiting for an answer that never comes, which
    /// is a hang, not a degrade. The rule is about the *user's* keystrokes.
    fn accepts_input(&self, cx: &gpui::App) -> bool {
        let Some(ws) = self.workspace().map(|w| w.workspace) else {
            return true;
        };
        crate::ui::remote_workspace::workspace_accepts_input(cx, ws)
    }

    /// Everything a reconnect needs to know about this pane, read on the UI
    /// thread before the blocking half runs off it: which pane, and at what
    /// geometry to bring it back ("以新客户端的尺寸 Resize").
    ///
    /// The geometry is *this* client's current one, not the one the pane was
    /// recorded at — a laptop that reconnects to a workspace it left on a
    /// 4K monitor must not come back at 300 columns.
    pub fn relink_plan(&self) -> (u64, TermSize, u16, u16) {
        (
            self.pane_id,
            self.terminal.size(),
            self.cell_width.as_f32().round() as u16,
            self.line_height.as_f32().round() as u16,
        )
    }

    /// Adopt a stream that has already re-`Attach`ed to this pane.
    ///
    /// The title goes back to the neutral one: the pane wore
    /// "tty7 — process exited" only because the *link* died, and leaving that on
    /// a tab whose shell is demonstrably still running would be the UI lying
    /// about the very thing this reconnect just disproved. A real title arrives
    /// with the replay if the shell sets one.
    pub fn adopt_relink(
        &mut self,
        stream: crate::daemon::transport::Stream,
        route: &crate::terminal::PaneRoute,
        size: TermSize,
        cell_w: u16,
        cell_h: u16,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        self.terminal
            .adopt_relink(stream, route, size, cell_w, cell_h)?;
        self.title = "tty7".to_string();
        cx.notify();
        Ok(())
    }

    /// Let go of this pane's link without ending the pane.
    ///
    /// The takeover: another client attached, so this one stops being
    /// the workspace's session. The pane stays on screen, read-only, exactly as
    /// a dropped link leaves it — what must *not* happen is this client going on
    /// holding a stream to a workspace somebody else is now typing in.
    pub fn detach_link(&mut self, cx: &mut Context<Self>) {
        self.terminal.detach_link();
        cx.notify();
    }

    /// The pane's cwd *when it names a directory on this pane's own
    /// [`host`](Self::host)* — the accessor the git probe, the diff and the
    /// worktree actions go through.
    ///
    /// This is the generalisation of [`local_cwd`](Self::local_cwd), and the
    /// two answer differently in exactly one case. `local_cwd` asks "is this
    /// path on *this machine*", because its callers do something local with it:
    /// open a file, spawn a local shell, persist a cwd a local shell will be
    /// restored into. This one asks "is this path on the machine I would ask
    /// about it", which is the weaker and more useful question — a pane whose
    /// shell runs over SSH has a perfectly good cwd, it just isn't here.
    ///
    /// The gate is *not* "is the pane remote". It is "does the pane's host
    /// agree with where the pane's shell runs": a remote pane still paired with
    /// the local host has a cwd nobody in this process can answer for, and
    /// handing `/home/me/proj` to a local `git` is the collision that gate
    /// exists to prevent (worse than useless on Windows, where that path is
    /// drive-relative and resolves to `C:\home\me\proj`). A remote-workspace
    /// pane does have its own host, so for it this returns the remote path and
    /// every caller below answers about the remote repository.
    pub fn host_cwd(&self) -> Option<std::path::PathBuf> {
        self.cwd_is_on_host().then(|| self.cwd())?
    }

    /// Whether paths this pane reports are in [`host`](Self::host)'s namespace.
    /// See [`host_cwd`](Self::host_cwd) for why this is not `remote_context()
    /// .is_none()`.
    ///
    /// "Runs elsewhere" is [`paths_are_local`](Self::paths_are_local) negated,
    /// **not** `remote_context().is_some()`. A remote-workspace pane's shell is
    /// perfectly local *to the machine it runs on*, so the `tty7-server` there
    /// reports no remote context for it — the fact that it is elsewhere is
    /// carried by the workspace, which is the other half of that accessor.
    fn cwd_is_on_host(&self) -> bool {
        cwd_is_on_host(!self.paths_are_local(), self.host_id.is_local())
    }

    /// The coding agent running in this pane's foreground, or `None` when none
    /// is. Identity comes from the daemon's foreground-`argv` detection (plus
    /// the sentinel event channel, which can brand wrappers argv can't see
    /// through). The tab avatar brands the pane with it. See
    /// [`crate::core::cli_agent`].
    pub fn agent(&self) -> Option<crate::core::cli_agent::CLIAgent> {
        self.terminal.foreground_agent()
    }

    /// The agent's rich session status (idle / working / waiting / done +
    /// native session id), when the pane's agent reports events over the
    /// sentinel OSC channel (or the opaque notification fallback). Drives the
    /// avatar's status dot, "needs your input" notifications, and resume. An
    /// output-idle *guess* is deliberately still absent — agents are quietest
    /// while thinking, so only agent-reported state is trusted.
    pub fn agent_session(&self) -> Option<crate::core::cli_agent::AgentSessionState> {
        self.terminal.agent_session()
    }

    /// Whether this pane's finished turn (the green `Done` dot) is unread — a
    /// turn ended that the user hasn't looked at since (see
    /// [`agent_result_unread`](Self::agent_result_unread) field). Feeds the
    /// tab's unread count (the avatar dot's count badge); the dot itself shows
    /// for any `Done`.
    pub fn agent_result_unread(&self) -> bool {
        self.agent_result_unread
    }

    /// Re-flag this pane's finished turn as unread — the tab context menu's
    /// "Mark as Unread". `refocus_incoming` is true for the pane the dismissed
    /// menu is about to hand window focus back to (the active tab's focused
    /// leaf): that focus-in is the menu closing, not the user reading the
    /// result, so it must not clear the mark it just made.
    pub fn mark_agent_result_unread(&mut self, refocus_incoming: bool) {
        self.agent_result_unread = true;
        self.keep_unread_on_focus = refocus_incoming;
    }

    /// The git snapshot for this pane's cwd (branch + working-tree diff), for
    /// the sidebar row's branch line — read from the shared per-repo
    /// [`GitStatusCache`](crate::terminal::git_status::GitStatusCache), so
    /// every pane in one work tree reports the same numbers. `None` outside a
    /// git work tree or before the repo's first background probe lands.
    pub fn git_status(&self, cx: &App) -> Option<crate::terminal::git_status::GitStatus> {
        let cwd = self.git_status_cwd.as_ref()?;
        cx.try_global::<crate::terminal::git_status::GitStatusCache>()?
            .status_for(self.host_id, cwd)
    }

    /// The cwd the pane's git line reads from — the same path [`git_status`]
    /// resolves through, so the diff overlay opened from that line probes the
    /// identical repo (not a fresh foreground-cwd syscall that could disagree
    /// mid-command). `None` outside a repo-probe-worthy state.
    ///
    /// [`git_status`]: Self::git_status
    pub fn git_status_cwd(&self) -> Option<&std::path::Path> {
        self.git_status_cwd.as_deref()
    }

    /// Re-probe this pane's git status opportunistically — for callers holding
    /// a reason to suspect the tree moved without the pane seeing it. The one
    /// that matters is the window regaining focus: edits made in an editor, or
    /// by a `git` command run in another app entirely, produce no event here at
    /// all, so without this the counts would sit stale until the user happened
    /// to run something in the pane.
    ///
    /// Throttled and in-flight-deduped (see [`GitRefresh::Opportunistic`]), so
    /// calling it for every pane on every activation is cheap. A pane with no
    /// resolved cwd yet is skipped rather than being pinned to `None` — its
    /// first real probe is the poll loop's job.
    pub fn refresh_git_status_now(&mut self, cx: &mut Context<Self>) {
        let cwd = self.git_status_cwd.clone();
        if cwd.is_some() {
            self.refresh_git_status(cwd, GitRefresh::Opportunistic, cx);
        }
    }

    /// The current grid selection as text, if any non-blank one exists — the
    /// source for "Agent: Send Selection".
    pub fn selection_text(&self) -> Option<String> {
        self.terminal
            .term
            .lock()
            .selection_to_string()
            .filter(|t| !t.trim().is_empty())
    }

    /// Deliver a built prompt into this pane's PTY as a bracketed paste + CR —
    /// the submit path for the agent context-feed commands. See
    /// [`crate::core::agent_prompt::submit_bytes`].
    pub fn send_agent_prompt(&self, prompt: &str) {
        self.terminal
            .write(crate::core::agent_prompt::submit_bytes(prompt));
    }

    /// Type one command line + Enter into the pane's PTY, as if the user had.
    /// Used by session restore to hand a fresh shell an agent resume command;
    /// the bytes queue in the PTY until the (possibly still-starting) shell
    /// reads them.
    pub fn run_command_line(&self, cmd: &str) {
        self.terminal.write(format!("{cmd}\r").into_bytes());
    }

    /// The shell this pane was explicitly spawned with (new-tab dropdown pick),
    /// so splits can inherit it. `None` → the default shell.
    pub fn shell_spec(&self) -> Option<ShellSpec> {
        self.shell_spec.clone()
    }

    /// The secret-free native-SSH spec this pane ran, if it is a native-SSH pane.
    /// Persisted for session restore and re-used by the in-pane reconnect
    /// (`RestartSshSession`).
    pub fn ssh_spec(&self) -> Option<Box<crate::daemon::protocol::NativeSshSpec>> {
        self.ssh_spec.clone()
    }

    /// The native-SSH connection phase for the status strip (PRD FR-E1); `None`
    /// for a non-native pane.
    pub fn ssh_phase(&self) -> Option<crate::daemon::protocol::SshPhase> {
        self.terminal.ssh_phase()
    }

    /// Whether this native-SSH pane's connection is dead (shell exited or the
    /// connect failed) and so eligible for an in-pane reconnect. False for live
    /// panes and non-native panes.
    pub fn ssh_disconnected(&self) -> bool {
        self.ssh_spec.is_some() && self.terminal.exited
    }

    fn handle_event(&mut self, ev: AlacEvent, cx: &mut Context<Self>) {
        // Surface a child-exit/daemon-disconnect noticed by the reader thread into
        // the field the view reads directly (`self.terminal.exited`).
        self.terminal.poll_exited();
        // A native-SSH pane may have queued an auth/host-key prompt behind this
        // wakeup; let the app drain it into the in-pane sheet. Cheap check —
        // only true during the brief pre-Output auth window.
        if self.terminal.has_pending_auth() {
            cx.emit(AuthPromptReady);
        }
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
                // Say which of the two things happened. For a local pane both
                // read the same and the wording is unchanged; for a remote
                // workspace they are opposite facts, and "process exited" on a
                // pane whose shell is still running on the far machine is the
                // one claim the degrade must not make — the whole
                // promise is that the work is still there when the link returns.
                self.title = if self.workspace().is_some() && !self.terminal.child_exited() {
                    "tty7 — disconnected".to_string()
                } else {
                    "tty7 — process exited".to_string()
                };
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
            AlacEvent::Bell => match cx.global::<Config>().bell {
                // Silenced: neither flash nor sound.
                BellMode::None => {}
                // Visual bell: a brief flash instead of an audible beep.
                BellMode::Visual => self.flash_bell(cx),
                // Audible bell: ring the system bell. Where none exists (non-mac
                // today), fall back to the flash so an opted-in bell is never
                // silent.
                BellMode::Audible => {
                    if !ring_system_bell() {
                        self.flash_bell(cx);
                    }
                }
            },
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
        // `terminal.exited` is set by two very different things: the shell
        // genuinely ending, and the *link* dropping (the reader's teardown sets
        // the same flag). For a local pane those are the same story and this
        // early return is unchanged — a local pane's link only dies when its
        // daemon does.
        //
        // For a remote-workspace pane they are not. The read-only
        // degrade is precisely the case where the link is gone and the shell is
        // not: that window must keep scrolling, selecting, copying and
        // searching, and every one of those runs below this line. What must not
        // happen — a keystroke reaching the machine — is stopped further down at
        // [`accepts_input`](Self::accepts_input), and again by the `exited`
        // checks on `send_to_pty` / `commit_text` themselves.
        let link_dropped_on_a_remote_pane =
            self.workspace().is_some() && !self.terminal.child_exited();
        if self.terminal.exited && !link_dropped_on_a_remote_pane {
            return;
        }
        // Any keystroke dismisses a visible integration notice — it has been
        // read. The Ctrl+R that raises it runs later in this same dispatch, so
        // the raising chord never clears its own notice.
        if self.integration_notice.take().is_some() {
            cx.notify();
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

        // The read-only degrade, placed **here and not at the top of
        // this function**.
        //
        // Everything above is the window's own keyboard, not the machine's:
        // ⌘F opens the search bar, ⌘A selects, ⌘C copies, ⌘1-9 switches tabs.
        // Every one of those keeps working while the link is
        // down — "能滚历史、能选能复制、能 ⌘F 搜索" — and a gate at the top of
        // `on_key_down` would silently take them all away, turning a read-only
        // window into an inert one. (⌘V is not an exception that needs handling
        // here: it reaches [`paste`](Self::paste), which has its own gate.)
        //
        // Everything *below* ends up at the PTY: the local line editor whose
        // Enter ships the line, and the raw key encoder. That is the half that
        // must not reach a machine we are not attached to.
        if !self.accepts_input(cx) {
            return;
        }

        // On macOS all ordinary text goes out through the IME, never through
        // `key_char` — see `input::defer_to_ime` for why (gpui reconstructs
        // `key_char` from the virtual keycode, which is a lie for synthesized
        // events). Decline the key without consuming it and gpui hands the
        // native event to the input context, which delivers the real text via
        // `commit_text`.
        //
        // Kitty's REPORT_ALL_KEYS_AS_ESC is the exception — `defer_to_ime`
        // declines under it so the key reaches the encoder below.
        //
        // A pending multi-key chord is already handled before this point: a key
        // that completes a sequence is dispatched as an action and never
        // reaches `on_key_down`. The check below is belt-and-braces (gpui takes
        // `pending_input` earlier in `dispatch_key_event`, so it never fires
        // here) and mirrors `prefers_ime_for_printable_keys`, which *is* live.
        #[cfg(target_os = "macos")]
        if !window.has_pending_keystrokes() && super::input::defer_to_ime(ks, self.kitty_flags()) {
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

        // Ctrl+R reaching this raw path means the tty7 history menu the user is
        // probably reaching for cannot appear here. When that's because shell
        // integration never engaged — not because a foreground command owns the
        // PTY — say so once instead of failing silently (#46). The chord still
        // goes to the PTY below, so the shell's own reverse-i-search keeps
        // working as the fallback.
        // Nothing to explain when the user switched the menu off — Ctrl+R
        // reaching the PTY is then exactly what they asked for.
        if m.control
            && !m.platform
            && !m.alt
            && ks.key == "r"
            && cx.global::<Config>().history_search
        {
            self.note_integration_gap(cx);
        }

        let kitty = self.kitty_flags();
        if let Some(bytes) = super::input::keystroke_to_bytes(ks, kitty) {
            let plain = !m.control && !m.alt && !m.platform;
            let shell_owns_prompt = self.shell_owns_prompt();
            // A plain Backspace is reconstructable gap input: offer it to the
            // hold, so a fast command's typeahead never touches the PTY (see
            // `hold`). Anything else releases the hold first — FIFO order on
            // the wire — and goes raw, kept in step with the typeahead record
            // for the deferred wipe.
            let held = plain
                && ks.key == "backspace"
                && !shell_owns_prompt
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
                if !shell_owns_prompt {
                    self.typeahead.observe(
                        RawInput::Key {
                            key: ks.key.as_str(),
                            plain,
                        },
                        self.on_alt_screen(),
                    );
                }
            }
            // Keep the cursor solid while typing (resets the blink phase).
            self.cursor_visible = true;
            // Typing clears the selection and jumps to the prompt.
            self.jump_to_prompt();
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
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> CmdKey {
        let m = &ks.modifiers;
        match ks.key.as_str() {
            // Copy / cut / paste route through the same methods the `CopyText` /
            // `CutText` / `PasteText` actions call, so the chord, the right-click
            // row and the Edit menu can't drift apart.
            "c" => {
                // `clear_on_copy`: Ctrl+C is dual-purpose — copy with a
                // selection, ^C (SIGINT) without — so the copy must consume the
                // selection or the next press copies again instead of
                // interrupting (#111). ⌘C never doubles as SIGINT, so there the
                // selection stays highlighted (the macOS convention).
                if self.copy_contextual(m.control, cx) {
                    CmdKey::Consumed
                } else {
                    // Nothing was selected anywhere: don't swallow the key, so
                    // Ctrl+C still reaches the PTY as ^C.
                    CmdKey::FallThrough
                }
            }
            "x" => {
                // Cut is editor-only; outside the prompt there is nothing to
                // remove, so the key falls through rather than dying silently.
                if self.cut_contextual(cx) {
                    CmdKey::Consumed
                } else {
                    CmdKey::FallThrough
                }
            }
            "v" => {
                self.paste_from_clipboard(cx);
                CmdKey::Consumed
            }
            // Find (open bar) and ⌘G / ⌘⇧G (next / previous match) are registered
            // keybindings — `FindInTerminal` / `FindNext` / `FindPrevious` — so they
            // are visible and rebindable in Settings and get a working default on
            // every platform (⌘F on macOS, Ctrl+Shift+F elsewhere). They dispatch
            // through `on_action`, not this inline path.
            "a" => {
                // At the prompt, ⌘A selects the whole edited line; otherwise it
                // selects the whole terminal buffer (scrollback included).
                self.select_all_contextual(cx);
                CmdKey::Consumed
            }
            // The following are editor-only (macOS line editing); they're swallowed
            // elsewhere since they have no terminal meaning.
            "z" => {
                self.undo_edit(m.shift, cx);
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
                    self.close_completion();
                    self.cursor_visible = true;
                    cx.notify();
                }
                CmdKey::Consumed
            }
            "delete" => {
                // ⌘⌫ deletes to line start; ⌘⌦ is its mirror — delete to line end.
                if self.input_active() {
                    if !self.cmd.delete_selection() {
                        self.cmd.delete_to_end();
                    }
                    self.close_completion();
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
        // The raw key path does this per keystroke; the editor owns the keyboard
        // at the prompt and every arm below returns early, so it has to happen
        // once here instead. Without it a key pressed while scrolled up edits a
        // line the viewport isn't showing (#208).
        self.jump_to_prompt();

        // ⌃P / ⌃N are readline's spelling of ↑ / ↓ (0x10 / 0x0e on the wire, and
        // what the shell's own keymap answers when the editor isn't holding the
        // line). Rewrite them into the arrow keys here rather than giving them
        // arms of their own, so the two spellings can't drift apart — history
        // recall, multi-line steps, the completion picker and the reverse-search
        // menu all treat them identically from this point down.
        let aliased;
        let ks = if m.control && !m.platform && !m.alt && matches!(key, "p" | "n") {
            aliased = gpui::Keystroke {
                modifiers: gpui::Modifiers::default(),
                key: if key == "p" { "up" } else { "down" }.to_string(),
                key_char: None,
            };
            &aliased
        } else {
            ks
        };
        let m = &ks.modifiers;
        let key = ks.key.as_str();

        // Any key other than a vertical step drops the sticky goal column, so the
        // next ↑/↓ takes its column from wherever the caret ends up.
        if key != "up" && key != "down" {
            self.editor_goal_col = None;
        }
        // Likewise, only a repeat of ⌥. continues an insert-last-word walk —
        // anything else and the next press starts fresh at the newest entry
        // rather than swallowing whatever now sits left of the caret.
        if !(m.alt && key == ".") {
            self.last_word_nav = None;
        }

        // A reverse search, when active, owns the keyboard.
        if self.reverse_search.is_some() {
            self.handle_reverse_search_key(ks, cx);
            return;
        }

        // ⌃J / ⌃M are readline's accept-line — the terminal's own encoding of
        // Enter (LF / CR), and what most shells' default keymaps bind. Route
        // them through the same path Enter takes, completion picker included.
        // Left alone they reach `apply_readline_ctrl`'s no-op arm and the Ctrl
        // branch below swallows them, so the key does nothing at all (#163).
        if m.control && !m.platform && !m.alt && matches!(key, "j" | "m") {
            self.accept_line(cx);
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
                    self.accept_line(cx);
                    return;
                }
                (true, "enter") => {
                    // Cmd+Enter: accept the highlighted candidate and run it.
                    self.completion_accept(cx);
                    self.submit_command(cx);
                    return;
                }
                (false, "escape") => {
                    self.close_completion();
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
        self.close_completion();

        // Readline-style control combinations, delegated so this dispatcher stays
        // scannable. A chord the editor answers is consumed here; one it doesn't
        // goes on to the shell rather than dying at the prompt. Either way this
        // branch returns.
        if m.control && !m.platform && !m.alt {
            // Off macOS, word navigation and deletion live on Ctrl (the Windows /
            // Linux convention): Ctrl+←/→ move by word (Shift extends the
            // selection), Ctrl+⌫/⌦ delete a word. macOS keeps these on Alt (handled
            // below) — its Ctrl+arrows are OS-level Space switches, and Ctrl+letters
            // stay readline — so claim the arrow / delete keys only off macOS.
            if cfg!(not(target_os = "macos")) {
                match key {
                    "left" => {
                        self.editor_move_h(false, m.shift, true);
                        cx.notify();
                        return;
                    }
                    "right" => {
                        self.editor_move_h(true, m.shift, true);
                        cx.notify();
                        return;
                    }
                    "backspace" => {
                        if !self.cmd.delete_selection() {
                            self.cmd.delete_word_left();
                        }
                        self.history_nav = None;
                        cx.notify();
                        return;
                    }
                    "delete" => {
                        if !self.cmd.delete_selection() {
                            self.cmd.delete_word_right();
                        }
                        cx.notify();
                        return;
                    }
                    _ => {}
                }
            }
            // Off macOS, Ctrl is the primary modifier, so Ctrl+A is expected to
            // select the whole edited line (text-editor / Windows convention) —
            // there is no reachable Cmd key to carry the macOS `Cmd+A`. macOS keeps
            // the readline `Ctrl+A` = move-to-line-start (its select-all is Cmd+A).
            if cfg!(not(target_os = "macos")) && key == "a" {
                self.cmd.select_all();
                self.close_completion();
                self.cursor_visible = true;
                cx.notify();
                return;
            }
            // With the history menu switched off ⌃R belongs to the shell: hand
            // the line over and let whatever is bound there answer — zle /
            // readline's own reverse-i-search, or an fzf / percol widget (#163).
            if key == "r" && !cx.global::<Config>().history_search {
                self.handoff_line_to_shell(&[0x12], cx);
                return;
            }
            if self.apply_readline_ctrl(key) {
                cx.notify();
            } else if let Some(bytes) = super::input::keystroke_to_bytes(ks, self.kitty_flags()) {
                // No local widget answers this chord. Swallowing it is the one
                // thing we mustn't do — the key worked before shell integration
                // engaged, and zle's keymap (⌃T transpose, a `bindkey` widget,
                // an fzf binding…) still knows what to do with it.
                self.handoff_line_to_shell(&bytes, cx);
            } else {
                cx.notify();
            }
            return;
        }

        // Readline-style Meta chords on the edited line: M-b / M-f motions,
        // M-d delete-word (mirroring the Alt+←/→/Delete handling below) and
        // M-. insert-last-word. On macOS these are reachable only with
        // `macos_option_as_alt` on — with it off the chord composes a character
        // upstream and arrives here altless, through the printable-text arm.
        // Meta chords with no arm here reach the shell instead of dying (see
        // the fallthrough at the bottom of the dispatcher).
        if m.alt && !m.platform && !m.control {
            match key {
                "." => {
                    self.insert_last_word(cx);
                    return;
                }
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
                // Any Enter that reaches here submits. The soft newline that
                // Shift+Enter / Opt+Enter authors is not handled inline: it is
                // the `InsertNewline` action, dispatched by the keymap before
                // the key ever reaches this dispatcher, so the chord can be
                // rebound like every other action (#182).
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
                // Within a multi-line buffer ↑ moves up a visual row; from the
                // top row it recalls the previous history entry.
                if self.editor_move_v(false, m.shift) {
                    cx.notify();
                } else {
                    self.history_prev(cx);
                }
                return;
            }
            "down" => {
                // The mirror of ↑: down a visual row, or newer history from the
                // bottom row.
                if self.editor_move_v(true, m.shift) {
                    cx.notify();
                } else {
                    self.history_next(cx);
                }
                return;
            }
            "escape" => {
                // Esc carries no local-editor meaning, so pass it straight to the
                // shell — its own zle/readline bindings act on it (vi command
                // mode from bindkey/readline vi mode, `\e`-prefixed widgets,
                // menu-select cancel). Shell vi-mode itself disables the local
                // editor from prompt start, so this is only the emacs-mode
                // fallback path.
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
            // `key_char`).
            _ => {
                if !m.control && !m.platform && !m.alt {
                    if let Some(ch) = ks.key_char.as_deref() {
                        if !ch.is_empty() && ch.chars().all(|c| c >= '\u{20}' && c != '\u{7f}') {
                            self.commit_text(ch, cx);
                            return;
                        }
                    }
                }
                // A Meta chord with nothing local behind it (M-t transpose-word,
                // M-u/M-l/M-c case widgets, whatever the user bound) goes to the
                // shell rather than dying here — same reasoning as the Ctrl side
                // above. The shared encoder goes first (it knows the shifted
                // character and the Kitty form when `key_char` is there to
                // consult), but the platforms that deliver Alt chords at all
                // don't reliably carry one — then fall back to ESC + the key
                // name, uppercased under Shift, as a raw terminal would send.
                if m.alt && !m.control && !m.platform && key.chars().count() == 1 {
                    let bytes = super::input::keystroke_to_bytes(ks, self.kitty_flags())
                        .unwrap_or_else(|| {
                            let name = if m.shift {
                                key.to_uppercase()
                            } else {
                                key.to_string()
                            };
                            let mut b = vec![0x1b];
                            b.extend_from_slice(name.as_bytes());
                            b
                        });
                    self.handoff_line_to_shell(&bytes, cx);
                    return;
                }
            }
        }
        cx.notify();
    }

    /// Apply a readline-style Ctrl chord to the command editor: Ctrl-A/E/B/F
    /// motions (Ctrl-F also accepts the autosuggestion), Ctrl-W/U/K/H deletions
    /// (each removing the selection first if there is one), Ctrl-Y yanking the
    /// last kill back, Ctrl-L clear-screen, Ctrl-R reverse search, Ctrl-C
    /// interrupt, and Ctrl-D EOF/forward-delete.
    ///
    /// Returns whether the chord was recognized: the caller hands the ones that
    /// weren't to the shell, so a widget tty7 has no answer for still reaches
    /// the keymap that does.
    ///
    /// The caller resolves Ctrl-J / Ctrl-M (accept-line), Ctrl-P / Ctrl-N (the
    /// arrow keys by another name) and, when the history menu is switched off,
    /// Ctrl-R before this point — none of them reach here.
    fn apply_readline_ctrl(&mut self, key: &str) -> bool {
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
            // Yank: the other half of ⌃W / ⌃U / ⌃K. Answered locally rather
            // than handed to the shell — zle keeps its own kill ring, and
            // yanking from it would paste text this editor never cut.
            "y" => self.cmd.yank(),
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
            _ => return false,
        }
        true
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

    /// Vertical caret motion across a multi-line / wrapped input buffer (↑/↓),
    /// with a sticky goal column so passing through a short line keeps the
    /// target column. Returns `true` if the caret moved within the buffer;
    /// `false` means it was already on the top row (↑) or bottom row (↓), so the
    /// caller falls through to history recall — matching how fish/zsh edit a
    /// multi-line line. Shift extends the selection.
    fn editor_move_v(&mut self, down: bool, shift: bool) -> bool {
        let Some((_, scol)) = self.cursor_cell() else {
            return false;
        };
        let cols = self.terminal.term.lock().columns().max(1);
        let chars: Vec<char> = self.cmd.text().chars().collect();
        let len = chars.len();
        let (positions, _r, _c) = input_char_positions(&chars, scol, cols);
        // The caret renders on the cell of the char it sits before, or on a
        // trailing slot at the buffer end (a fresh row when the buffer ends in a
        // newline).
        let end_caret = if len == 0 {
            (0usize, scol)
        } else {
            let (r, c, w) = positions[len - 1];
            if chars[len - 1] == '\n' {
                (r + 1, 0)
            } else {
                (r, c + w)
            }
        };
        let (cur_row, cur_col) = if self.cmd.cursor() < len {
            let (r, c, _) = positions[self.cmd.cursor()];
            (r, c)
        } else {
            end_caret
        };
        let mut max_row = positions.iter().map(|&(r, _, _)| r).max().unwrap_or(0);
        if chars.last() == Some(&'\n') {
            max_row += 1;
        }
        // On the boundary row in the travel direction, defer to history recall.
        if (down && cur_row >= max_row) || (!down && cur_row == 0) {
            self.editor_goal_col = None;
            return false;
        }
        let target = if down { cur_row + 1 } else { cur_row - 1 };
        let goal = *self.editor_goal_col.get_or_insert(cur_col);
        // Land on the caret slot of the target row nearest the goal column. Char
        // `i`'s slot is the caret *before* it; the buffer-end slot is `len`.
        let mut best: Option<(usize, usize)> = None; // (index, |col - goal|)
        for (i, &(r, c, _)) in positions.iter().enumerate() {
            if r == target {
                let dist = c.abs_diff(goal);
                if best.is_none_or(|(_, bd)| dist < bd) {
                    best = Some((i, dist));
                }
            }
        }
        if end_caret.0 == target {
            let dist = end_caret.1.abs_diff(goal);
            if best.is_none_or(|(_, bd)| dist < bd) {
                best = Some((len, dist));
            }
        }
        let Some((idx, _)) = best else {
            return false;
        };
        if shift {
            self.cmd.begin_selection();
        } else {
            self.cmd.clear_selection();
        }
        self.cmd.set_cursor(idx);
        true
    }

    fn has_selection(&self) -> bool {
        self.terminal.term.lock().selection.is_some()
    }

    /// Is there anything [`copy_contextual`](Self::copy_contextual) would copy —
    /// in the grid *or* in the prompt editor? What the Copy / Cut menu rows gate
    /// on: `has_selection` alone is grid-only, so a prompt selection used to
    /// leave "Copy" greyed out even though ⌘C would have copied it.
    fn any_selection(&self) -> bool {
        self.has_selection() || (self.input_active() && self.cmd.selected_text().is_some())
    }

    /// Snapshot the Kitty keyboard-protocol flags the app has enabled, read off the
    /// local `Term`'s mode bits (the reader thread keeps them current by advancing
    /// the emulator over all child output). Consulted by the key encoder so TUIs
    /// that opt into the protocol get `CSI u` reports.
    pub(super) fn kitty_flags(&self) -> super::input::KittyFlags {
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

    /// The housekeeping every input path shares: drop the selection the key
    /// invalidated and bring the viewport back to the live prompt, whole lines
    /// (`display_offset`) and sub-line remainder (`scroll_frac`) alike. Acting
    /// on a line the user can't see is the thing to avoid — so this runs for
    /// keys handled locally too, not only for bytes that reach the PTY.
    fn jump_to_prompt(&mut self) {
        let mut term = self.terminal.term.lock();
        term.selection = None;
        term.scroll_display(Scroll::Bottom);
        drop(term);
        self.scroll_frac = 0.;
    }

    /// Write a fixed byte sequence to the PTY (for keystrokes delivered as
    /// actions rather than through `on_key_down`, e.g. Tab / Shift-Tab), applying
    /// the same cursor / selection / scroll housekeeping as normal typing.
    fn send_to_pty(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        if self.terminal.exited || !self.accepts_input(cx) {
            return;
        }
        self.terminal.write(bytes.to_vec());
        self.cursor_visible = true;
        self.jump_to_prompt();
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
        if !self.accepts_input(cx) {
            return;
        }
        if self.input_active() {
            let trimmed = text.strip_suffix('\n').unwrap_or(&text);
            self.cmd.insert_str(trimmed);
            self.history_nav = None;
            self.editor_goal_col = None;
            self.close_completion();
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
        // Pasting to the PTY is input like typing: it consumes the selection,
        // so a following Ctrl+C means ^C again (#111). The editor branch above
        // leaves the selection alone, matching `commit_text`.
        self.terminal.term.lock().selection = None;
        cx.notify();
    }

    // ---- Mouse tracking (so vim / tmux / zellij get clicks & drags) ----

    /// True when the application has enabled any mouse-reporting mode.
    /// Drive the momentary visual bell flash: turn it on now, then schedule a
    /// one-shot task to clear it ~150ms later. Shared by the `Visual` bell mode
    /// and the `Audible` fallback on platforms without a system bell.
    fn flash_bell(&mut self, cx: &mut Context<Self>) {
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

    pub fn mouse_mode(&self) -> bool {
        self.report_mouse
            && self
                .terminal
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
        let wants = self.report_mouse
            && self
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
        if !self.report_mouse
            || !self
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
        let mut mode = *self.terminal.term.lock().mode();
        // "Mouse reporting off" also silences the wheel: drop the report mode so
        // the tick falls through to alternate-scroll / local scrollback, exactly
        // as if the app had never asked for wheel reporting.
        if !self.report_mouse {
            mode.remove(TermMode::MOUSE_MODE);
        }
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

    /// Copy whatever is selected, preferring the prompt editor's selection over
    /// the terminal grid's. Returns whether anything was actually copied — the
    /// ⌃C path needs to know, because with nothing selected the key has to fall
    /// through to ^C (SIGINT).
    ///
    /// `clear_on_copy` drops the selection after copying. Ctrl+C is dual-purpose
    /// (copy with a selection, SIGINT without), so it must consume the selection
    /// or the next press copies forever instead of interrupting (#111); ⌘C and
    /// the menu items leave the highlight up, the macOS convention.
    ///
    /// The single copy path: ⌘C / ⌃C, the right-click "Copy" row, and the Edit
    /// menu all land here.
    pub fn copy_contextual(&mut self, clear_on_copy: bool, cx: &mut Context<Self>) -> bool {
        // At the prompt the editor's selection wins — but only when it has one.
        // With no editor selection we fall on through: the user may have
        // mouse-selected terminal output/scrollback, which lives in
        // `term.selection`, not in the editor.
        if self.input_active() {
            if let Some(text) = self.cmd.selected_text() {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                if clear_on_copy {
                    self.cmd.clear_selection();
                    cx.notify();
                }
                return true;
            }
        }
        if self.has_selection() {
            self.copy_selection(cx);
            if clear_on_copy {
                self.terminal.term.lock().selection = None;
                cx.notify();
            }
            return true;
        }
        false
    }

    /// Step to the next (`forward`) or previous search match. A no-op while the
    /// find bar is closed — there is nothing to step through. Exposed for the
    /// palette's "Find Next" / "Find Previous", which run from outside the
    /// terminal module and so can't reach `step_match` directly.
    pub fn find_step(&mut self, forward: bool, cx: &mut Context<Self>) {
        let direction = if forward {
            Direction::Right
        } else {
            Direction::Left
        };
        self.step_match(direction, cx);
    }

    /// Undo (or, with `redo`, redo) the last prompt edit. Editor-only: the
    /// terminal grid has no edit history, so outside the prompt this is a no-op
    /// that still swallows the gesture rather than sending ⌘Z to the PTY.
    /// Shared by the ⌘Z chord and the Edit menu's Undo / Redo.
    pub fn undo_edit(&mut self, redo: bool, cx: &mut Context<Self>) {
        if !self.input_active() {
            return;
        }
        if redo {
            self.cmd.redo();
        } else {
            self.cmd.undo();
        }
        self.close_completion();
        cx.notify();
    }

    /// Cut the prompt editor's selection: copy it out, then delete it. Only
    /// meaningful at the prompt — the terminal grid is not editable — so this
    /// reports whether the gesture was *handled* (i.e. the prompt was active),
    /// not whether text was actually removed; a cut with nothing selected is
    /// still a no-op the prompt owns rather than a key the PTY should see.
    pub fn cut_contextual(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.input_active() {
            return false;
        }
        if let Some(text) = self.cmd.selected_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            self.cmd.delete_selection();
            self.close_completion();
            self.cursor_visible = true;
            cx.notify();
        }
        true
    }

    /// Read the system clipboard and paste it into the PTY (bracketed-paste
    /// aware). The single paste path: ⌘V / ⌃V, the right-click "Paste" row, and
    /// the Edit menu.
    ///
    /// Text wins when the clipboard carries any. Failing that — an image-only
    /// clipboard (a screenshot) dropped on a pane whose foreground app is a TUI
    /// coding agent — the image is written to a temp file and its path typed in,
    /// which is how those agents take attachments.
    pub fn paste_from_clipboard(&mut self, cx: &mut Context<Self>) {
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        if let Some(text) = clipboard_paste_text(&item) {
            self.paste(text, cx);
            return;
        }
        if self.input_active() {
            return;
        }
        if let Some(img) = item.entries().iter().find_map(|e| match e {
            ClipboardEntry::Image(img) => Some(img),
            _ => None,
        }) {
            self.paste_clipboard_image(img, cx);
        }
    }

    /// Files dragged in from Finder (etc.) and dropped on the terminal:
    /// shell-escape each path, join with spaces, and insert them like a paste —
    /// with a trailing space so a dropped path is ready to be an argument and
    /// back-to-back drops don't run together. Matches macOS Terminal.app
    /// (which reuses its paste escaping for drops).
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

    /// Paste a clipboard image (e.g. a screenshot) into a foreground coding-agent
    /// TUI. Agents like Claude Code attach an image typed as a *file path* at the
    /// prompt — the same route drag-and-drop uses — so off macOS we stage the image
    /// to a temp file and paste its shell-escaped path, mirroring [`drop_files`].
    ///
    /// On macOS the agent can instead read the image straight from the pasteboard
    /// when it sees Ctrl+V, so we forward SYN (`0x16`) and let it do that
    /// higher-fidelity read. That same read is unreliable off macOS — Claude Code on
    /// Windows silently drops raw screenshots (anthropics/claude-code#26679) — which
    /// is why we materialize a file there. If staging fails, we fall back to SYN.
    fn paste_clipboard_image(&mut self, img: &gpui::Image, cx: &mut Context<Self>) {
        #[cfg(not(target_os = "macos"))]
        if let Some(path) = write_clipboard_image(img) {
            let text = shell_escape_path(&path.to_string_lossy());
            self.paste(format!("{text} "), cx);
            return;
        }
        let _ = img;
        self.terminal.write(vec![0x16]);
        // PTY input consumes the selection, like `paste` (#111).
        self.terminal.term.lock().selection = None;
        cx.notify();
    }

    /// Clear the terminal (right-click "Clear"), like Cmd+K / the `clear`
    /// command: purge the scrollback history *and* wipe the visible screen.
    /// We drop the history directly, then send Ctrl+L so the shell/TUI repaints
    /// its prompt at the top with the cursor in sync (no desync from poking the
    /// grid behind the program's back).
    pub fn clear_scrollback(&mut self, cx: &mut Context<Self>) {
        self.terminal.term.lock().grid_mut().clear_history();
        self.scroll_frac = 0.;
        // Every mark's row indexed into the history that just went away, so the
        // Outline's positions are now meaningless. Drop them rather than leave
        // rows that scroll somewhere arbitrary.
        self.terminal.marks().clear();
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

        // A deferred history record is finalized once the shell has reported
        // back at its prompt: the daemon's `last_exit` is now this command's.
        // Sequence-based, so a fast command whose not-at-prompt window fell
        // between polls still gets its exit code.
        if self
            .pending_history
            .as_ref()
            .is_some_and(|p| at_prompt && self.terminal.prompt_seq() > p.seq)
        {
            self.flush_pending_history();
            cx.notify();
        }

        // Re-rank history when the working directory changes (a `cd`), so ghost text
        // and completion start favouring commands run in the new directory. Only on
        // a real, known change — an unknown cwd keeps the previous ranking.
        if let Some(cwd) = self.cwd()
            && self.ranked_cwd.as_ref() != Some(&cwd)
        {
            self.rerank_history(Some(&cwd));
        }

        // Shell integration engaging late (a slow rc file finally reported)
        // makes a visible integration notice wrong — retract it. The
        // once-per-pane latch stays set: the overlay works now, there is
        // nothing left to explain.
        if self.integration_notice.is_some() && self.terminal.shell_active() {
            self.integration_notice = None;
            cx.notify();
        }

        // Redraw when the prompt/running state flips, so the line editor shows or
        // hides promptly even when the shell produced no output to trigger a
        // repaint (e.g. a command that prints nothing). Without this the editor's
        // visibility — computed in `render` — could lag until the next redraw.
        if at_prompt != self.last_at_prompt {
            self.last_at_prompt = at_prompt;
            cx.notify();
        }

        // Whether the configured notification policy allows a post right now:
        // never / only-when-unfocused / always. Shared by the command-finished,
        // agent-finished, and agent-waiting notifications.
        let notify_allowed = match cx.global::<Config>().notify_on_command_finish {
            NotifyMode::Never => false,
            NotifyMode::Unfocused => !window.is_window_active(),
            NotifyMode::Always => true,
        };

        // "Command finished" notification: a foreground command (not at prompt)
        // that ran long and finished while the window was in the background. When
        // the command was a recognized coding agent, brand the notification with
        // the agent instead of the generic "command finished" copy.
        let running = !at_prompt;
        // While a command runs, latch the agent the daemon reports for it — the
        // detection poll can land a beat after the command starts, so capture it
        // whenever it appears rather than only at the start edge.
        if running && self.running_agent.is_none() {
            self.running_agent = self.terminal.foreground_agent();
        }
        // A command finishing (back-to-prompt edge) may have edited files or
        // switched branch, so reprobe git after it — captured before the match
        // below clears `running_since`.
        let cmd_finished = self.running_since.is_some() && !running;
        match (self.running_since, running) {
            (None, true) => {
                self.running_since = Some(std::time::Instant::now());
                self.running_title = self.title.clone();
                self.running_agent = self.terminal.foreground_agent();
            }
            (Some(start), false) => {
                let elapsed = start.elapsed();
                let title = std::mem::take(&mut self.running_title);
                let agent = self.running_agent.take();
                self.running_since = None;
                if notify_allowed {
                    match agent {
                        // A rich-channel agent already announced each turn's
                        // end (`stop` events below); a second "finished" on
                        // process exit would be noise.
                        Some(_) if self.agent_was_rich => {}
                        // An agent session ends the moment it finishes — no
                        // duration floor: "Claude Code finished" is worth saying
                        // even for a quick turn you stepped away from.
                        Some(agent) => notify_agent_finished(agent, elapsed),
                        None => {
                            let threshold = std::time::Duration::from_secs(
                                cx.global::<Config>().notify_threshold_secs,
                            );
                            if elapsed >= threshold {
                                notify_command_finished(&title, elapsed);
                            }
                        }
                    }
                }
            }
            _ => {}
        }

        let turn_finished = self.poll_agent_status(notify_allowed, cx);

        // Refresh the sidebar's git branch/diff line when the working directory
        // changed (a `cd`), a command just finished, or an agent turn ended —
        // an agent's session is one long foreground command, so its edits would
        // otherwise stay invisible until it exits. All rare edges, so the
        // off-thread `git` shell-out runs seldom, not every 300ms tick.
        //
        // Those edges alone left the counts badly stale during the case they
        // matter most: a long agent turn writes file after file for minutes
        // with nothing to show for it. A tool completion is the one signal that
        // the tree may have just moved mid-turn, so it refreshes too — through
        // the throttled path, since a busy agent emits them several a second
        // and each one would otherwise cost a `git diff` across the repo.
        //
        // An agent that reports its own cwd through the hook channel wins over
        // the proc probe: it tracks internal chdirs the PTY can't observe
        // (Claude Code's EnterWorktree) and works where the proc fallback
        // doesn't (Windows). The claim dies with the session (`session-end`
        // clears it, and the agent leaving the foreground drops the whole
        // state), so an exited agent falls back to the pane's real directory.
        // The agent's report goes through the same host gate as the pane's own
        // cwd. A native-SSH pane keeps sentinel-sourced agent state on purpose
        // (`spawn_native_ssh`), so an agent running *on the remote host* reports
        // a remote path — and being first in the chain it would win over the
        // pane's own cwd unconditionally and hand that path to a `git` that
        // cannot see it, which is the collision `cwd_is_on_host` prevents.
        let session = self.terminal.agent_session();
        // A count that moved means at least one tool finished since the last
        // tick. With no session the counter resets, so a fresh agent's very
        // first tool call still reads as activity.
        let tool_activity = match session.as_ref().map(|s| s.activity) {
            Some(n) => std::mem::replace(&mut self.last_agent_activity, n) != n,
            None => {
                self.last_agent_activity = 0;
                false
            }
        };
        let cwd_now = self
            .cwd_is_on_host()
            .then(|| {
                session
                    .as_ref()
                    .and_then(|s| s.cwd.clone())
                    .or_else(|| self.cwd())
            })
            .flatten();
        if cwd_now.as_ref() != self.git_status_cwd.as_ref() || cmd_finished || turn_finished {
            self.refresh_git_status(cwd_now, GitRefresh::Edge, cx);
        } else if tool_activity {
            self.refresh_git_status(cwd_now, GitRefresh::Opportunistic, cx);
        }
    }

    /// Kick off an off-thread git probe for `cwd` on this pane's
    /// [`host`](Self::host) and fold the result into the shared per-repo
    /// [`GitStatusCache`] on the main thread. The cache brackets the flight
    /// (`begin_probe`/`finish_probe`) per `(host, cwd)`: a probe already in
    /// flight for the same pair absorbs this trigger instead of spawning a
    /// duplicate, and reruns once when it lands. With no cwd the pane simply
    /// stops reading a status. Callers must source the cwd from
    /// [`host_cwd`](Self::host_cwd): a pane whose paths its host cannot answer
    /// for *does* get a cwd once its OSC 7 lands, so "such panes have no cwd"
    /// holds only before that and cannot be what keeps the probe away from a
    /// path it would misread.
    ///
    /// [`GitStatusCache`]: crate::terminal::git_status::GitStatusCache
    fn refresh_git_status(
        &mut self,
        cwd: Option<std::path::PathBuf>,
        trigger: GitRefresh,
        cx: &mut Context<Self>,
    ) {
        use crate::terminal::git_status::GitStatusCache;

        let changed = self.git_status_cwd != cwd;
        self.git_status_cwd = cwd.clone();
        let Some(cwd) = cwd else {
            if changed {
                cx.notify();
            }
            return;
        };
        let id = self.host_id;
        // A host that is not there cannot be probed, and a probe that fails
        // would replace a good branch line with nothing. Keep showing the last
        // answer instead — the reconnect fires a fresh trigger.
        //
        // Two ways for it not to be there, both landing here: the machine is
        // unregistered (its workspace closed, or this process never connected),
        // or it is registered but its connection is down.
        //
        // Still repaint if the cwd moved: `git_status_cwd` is what
        // `git_status()` resolves through, so leaving the frame unnotified
        // would keep the *previous* directory's branch line on screen until
        // some unrelated event happened to notify.
        let Some(host) = self.host(cx) else {
            if changed {
                cx.notify();
            }
            return;
        };
        if !host.is_connected() {
            if changed {
                cx.notify();
            }
            return;
        }
        cx.default_global::<GitStatusCache>(); // first probe of the process creates it
        let claimed = cx.update_global::<GitStatusCache, _>(|cache, _| match trigger {
            GitRefresh::Edge => cache.begin_probe(id, &cwd),
            GitRefresh::Opportunistic => {
                cache.begin_probe_throttled(id, &cwd, OPPORTUNISTIC_GIT_GAP)
            }
        });
        if !claimed {
            return;
        }
        let probe_cwd = cwd.clone();
        let pane = cx.weak_entity();
        // `run_detached`, not `run`: the result has to reach the shared cache
        // whether or not this pane outlives the probe. The claim is keyed by
        // `(host, cwd)`, so a pane closed mid-flight that never released its
        // claim would wedge the git line of every other pane in that directory
        // — permanently, since nothing else ever clears it.
        crate::ui::host_ops::HostOps::run_detached(
            host,
            cx,
            move |h| crate::terminal::git_status::probe(h, &probe_cwd),
            move |cx, result| {
                // Landing through `update_global` wakes the sidebar's
                // `observe_global`, so every pane in the repo repaints — not
                // just this one.
                let rerun = cx.update_global::<GitStatusCache, _>(|cache, _| {
                    cache.finish_probe(id, &cwd, result)
                });
                // A trigger arrived while we flew; go once more so its state is
                // observed — unless this pane has since left that cwd (or left
                // entirely). Only edge triggers set that flag, so the rerun is
                // an edge too.
                if rerun {
                    let _ = pane.update(cx, |view, cx| {
                        if view.git_status_cwd.as_deref() == Some(&cwd) {
                            view.refresh_git_status(Some(cwd), GitRefresh::Edge, cx);
                        }
                    });
                }
            },
        );
    }

    /// Fold the pane's rich agent status into turn-level notifications and the
    /// status dot. Runs on the same cadence as the notification poll above.
    ///
    /// Only *transitions* act: entering `Waiting` says the agent needs you
    /// (the reason attached), and a `Working → Done` edge says the turn
    /// finished — with its duration when we saw it start. Non-rich (fallback)
    /// state paints the dot but stays silent: the agent's own OSC notification
    /// was already toasted by the reader thread, and echoing it would double
    /// up. Attach replays land as a bare status with no observed transition
    /// history, so a restored `Done` never re-notifies.
    ///
    /// Returns whether a turn just ended (a transition *into* `Done`) — the
    /// caller uses it to reprobe git: an agent's whole session is one long
    /// foreground command, so the back-to-prompt edge that normally refreshes
    /// the branch/diff line never fires while it works.
    fn poll_agent_status(&mut self, notify_allowed: bool, cx: &mut Context<Self>) -> bool {
        use crate::core::cli_agent::AgentStatus;

        let session = self.terminal.agent_session();
        if session.as_ref().is_some_and(|s| s.rich) {
            self.agent_was_rich = true;
        }
        if self.terminal.foreground_agent().is_none() && session.is_none() {
            self.agent_was_rich = false;
        }

        // Ahead of the status early-return below, because this does not move
        // with the status: an id appears when the agent's hooks first report a
        // conversation, which is a moment the status has no opinion about.
        let identity = (
            session.as_ref().and_then(|s| s.session_id.clone()),
            session.as_ref().and_then(|s| s.launch_argv.clone()),
        );
        if identity != self.last_agent_session {
            self.last_agent_session = identity;
            cx.emit(AgentSessionChanged);
        }

        let status = session.as_ref().map(|s| s.status);
        if status == self.last_agent_status {
            return false;
        }
        let prev = std::mem::replace(&mut self.last_agent_status, status);
        let turn_finished = status == Some(AgentStatus::Done) && prev != Some(AgentStatus::Done);

        // Read/unread for the green Done dot: a turn just finished is "unread"
        // only if you weren't looking (focused pane = you watched it finish, so
        // it's already read). Any non-Done status has no result to be unread.
        match status {
            Some(AgentStatus::Done) if prev != Some(AgentStatus::Done) => {
                self.agent_result_unread = !self.focused;
                self.keep_unread_on_focus = false;
            }
            Some(AgentStatus::Done) => {}
            _ => {
                self.agent_result_unread = false;
                self.keep_unread_on_focus = false;
            }
        }

        let rich = session.as_ref().is_some_and(|s| s.rich);
        let agent_name = self
            .terminal
            .foreground_agent()
            .map(|a| a.display_name())
            .unwrap_or("Agent");
        match status {
            Some(AgentStatus::Working) => {
                self.agent_turn_started = Some(std::time::Instant::now());
            }
            Some(AgentStatus::Waiting) if rich && notify_allowed => {
                let body = session
                    .as_ref()
                    .and_then(|s| s.message.clone())
                    .unwrap_or_else(|| "Waiting for your input".to_string());
                super::remote::notify_desktop(Some(agent_name), &body);
            }
            // Done only counts off an *observed* turn (working/waiting seen
            // live), so an attach replay of old state stays quiet.
            Some(AgentStatus::Done)
                if rich
                    && notify_allowed
                    && matches!(
                        prev,
                        Some(AgentStatus::Working) | Some(AgentStatus::Waiting)
                    ) =>
            {
                let body = match self.agent_turn_started.take() {
                    Some(start) => format!("Finished after {}s", start.elapsed().as_secs()),
                    None => "Turn finished".to_string(),
                };
                super::remote::notify_desktop(Some(agent_name), &body);
            }
            _ => {}
        }
        // Status changed: repaint so the avatar dot / sidebar line track it.
        cx.notify();
        turn_finished
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

    /// How many rows the whole surface — grid and input overlay together —
    /// shifts up so a wrapped command at a bottom-of-screen prompt stays
    /// visible, emulating the scroll the shell itself would perform if the
    /// input were echoed. `element::paint` raises the grid origin by this many
    /// lines (clipping the top rows) and `render_input_bar` anchors the same
    /// rows higher, so the wrapped tail lands in the vacated strip. Zero
    /// whenever nothing overflows, while scrolled into history (the overlay is
    /// off-screen anyway and the view shouldn't fight the user's scroll), or
    /// in reverse-search mode (a single fixed row).
    pub(super) fn input_scroll_rows(&self) -> usize {
        if !self.input_active() || self.reverse_search.is_some() {
            return 0;
        }
        let Some((crow, ccol)) = self.cursor_cell() else {
            return 0;
        };
        let (rows, cols, offset) = {
            let term = self.terminal.term.lock();
            (
                term.screen_lines(),
                term.columns(),
                term.grid().display_offset(),
            )
        };
        if offset != 0 {
            return 0;
        }
        let chars: Vec<char> = self.cmd.text().chars().collect();
        let (visual_rows, caret_vrow) = input_overlay_rows(
            &chars,
            self.cmd.cursor(),
            &self.marked_text,
            ccol,
            cols.max(1),
        );
        input_overflow_shift(crow, caret_vrow, visual_rows, rows)
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
        shift: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(idx) = self.editor_char_index(col, row, false) else {
            return false;
        };
        match clicks {
            // Shift+click extends the selection from the current caret to the
            // click (anchoring one at the old caret if none is active), matching
            // shift-arrow selection; a plain click collapses to the caret.
            1 if shift => {
                self.cmd.extend_to(idx);
                self.editor_selecting = true; // a drag from here keeps extending
                self.editor_drag_word = None;
            }
            1 => {
                self.cmd.set_cursor(idx);
                self.cmd.clear_selection();
                self.editor_selecting = true; // a drag from here extends selection
                self.editor_drag_word = None;
            }
            2 => {
                let cfg = cx.global::<Config>();
                let (seps, smart) = (cfg.word_separators.clone(), cfg.smart_select);
                self.cmd.select_word_at(idx, &seps, smart);
                // Drag now grows the selection by whole words around this one.
                self.editor_selecting = true;
                self.editor_drag_word = self.cmd.selection();
            }
            _ => {
                self.cmd.select_all();
                // The whole line is selected; a drag has nothing left to extend.
                self.editor_selecting = false;
                self.editor_drag_word = None;
            }
        }
        self.editor_select_gesture = true;
        self.editor_goal_col = None;
        self.close_completion();
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
        // A drag begun on a double-click extends by whole words; otherwise by char.
        if let Some((s, e)) = self.editor_drag_word {
            let cfg = cx.global::<Config>();
            let (seps, smart) = (cfg.word_separators.clone(), cfg.smart_select);
            self.cmd.extend_word_to(s, e, idx, &seps, smart);
        } else {
            self.cmd.extend_to(idx);
        }
        self.cursor_visible = true;
        cx.notify();
        true
    }

    /// Whether the local line editor should be live and focused: idle at a shell
    /// prompt, not on the alternate screen, no search bar open, process alive.
    /// Everywhere else this is `false`, so the raw terminal keeps the keyboard and
    /// behaves exactly as without the editor.
    pub fn input_active(&self) -> bool {
        self.input_inactive_reason().is_none()
    }

    /// Why the line editor is standing down, phrased for a log line — `None`
    /// when it is live.
    ///
    /// The conditions live here rather than inline in [`input_active`] because
    /// "the editor didn't engage" is the shape almost every report of this
    /// feature takes ("Tab did nothing", "it fell back to the shell"), and six
    /// silent booleans are indistinguishable from the outside. One list, so the
    /// answer is a `TTY7_LOG=debug` away instead of a bisect.
    fn input_inactive_reason(&self) -> Option<&'static str> {
        if self.terminal.exited {
            return Some("the shell has exited");
        }
        // Suppress our command editor only while the search field actually holds
        // keyboard focus (it claims Tab / ↑ / ↓ / typing). If search is open but
        // blurred — e.g. the user clicked back into the terminal — the editor must
        // resume, otherwise keys fall through to the raw PTY path and can't be edited.
        if self.search_focused {
            return Some("the search field holds the keyboard");
        }
        if self.on_alt_screen() {
            return Some("the pane is on the alternate screen");
        }
        if self.shell_vi_prompt() {
            return Some("the shell prompt is in vi mode");
        }
        // A Tab handoff gave this prompt's line to the shell; until a command
        // runs and a fresh prompt cycle starts, the shell's editor owns it,
        // and re-engaging ours would fork the two line buffers.
        if self.editor_handoff == Some(self.terminal.prompt_cycle()) {
            return Some("this prompt's line was already handed to the shell");
        }
        if !self.at_shell_prompt() {
            return Some("the shell has not reported a prompt (no OSC 133)");
        }
        None
    }

    fn shell_vi_prompt(&self) -> bool {
        self.terminal.shell_vi_mode() && self.terminal.at_prompt() && !self.on_alt_screen()
    }

    /// True while a Tab handoff has given the current prompt's line to the
    /// shell (see [`Self::handoff_tab_to_shell`]) and the shell is still in
    /// that prompt cycle. Over once a command runs and the next prompt
    /// arrives (a false→true `at_prompt` edge bumps the cycle).
    fn handoff_active(&self) -> bool {
        self.editor_handoff == Some(self.terminal.prompt_cycle())
            && self.terminal.at_prompt()
            && !self.on_alt_screen()
    }

    /// True while the shell's own line editor owns the prompt line — a
    /// vi-mode prompt, or one whose line a Tab handoff shipped over. Raw
    /// input then goes to the PTY with no hold and no typeahead record:
    /// those bytes land on zle's line and are the shell's to keep, so a
    /// deferred `^U` wipe would erase text the user can see.
    fn shell_owns_prompt(&self) -> bool {
        self.shell_vi_prompt() || self.handoff_active()
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
        self.terminal.shell_active() && !self.on_alt_screen() && !self.shell_owns_prompt()
    }

    /// Write printable gap text (IME commit, paste) toward the shell: offered
    /// to the hold when reconstructable (see `hold`), otherwise released +
    /// written raw and recorded for the deferred wipe. `bytes` is the exact
    /// PTY encoding (paste may be bracketed-wrapped).
    fn write_gap_text(&mut self, text: &str, bytes: Vec<u8>, cx: &mut Context<Self>) {
        if self.shell_owns_prompt() {
            self.release_hold();
            self.terminal.write(bytes);
            return;
        }
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
        // The one gate that is not next to a keystroke. This runs off a timer
        // armed while the pane was still attached, so it can fire *after* the
        // link dropped and after every other check has already returned — held
        // bytes would then reach the machine as the one thing the read-only
        // degrade promises cannot happen. Nothing is buffered for later (D6):
        // the hold is dropped, not queued.
        if !self.accepts_input(cx) {
            let _ = self.hold.timeout(epoch);
            return;
        }
        if let Some((net, bytes)) = self.hold.timeout(epoch) {
            self.terminal.write(bytes);
            let alt = self.on_alt_screen();
            self.typeahead.observe(RawInput::Text(&net), alt);
            cx.notify();
        }
    }

    /// The `InsertNewline` action: insert a literal newline at the caret so the
    /// user can author (or extend) a multi-line command, which plain Enter then
    /// submits whole. Bound to Shift+Enter and Alt+Enter by default.
    ///
    /// Only the local command editor answers this. When the editor isn't holding
    /// the line — a foreground application owns the screen, the search field has
    /// focus — or while a reverse search owns the keyboard, we `propagate`
    /// instead, so the chord takes the exact path it took before this action
    /// existed: on to `on_key_down`, and from there to the widget or out to the
    /// application as raw bytes.
    ///
    /// An open completion menu deliberately does *not* decline it. A newline
    /// ends the word being completed, so the menu is closed and the newline
    /// inserted — for both chords. Warp draws the same line: only a bare Enter
    /// reaches the popup-acceptance path (`FixedBinding::new("enter", …)` →
    /// `input_enter`), while Shift+Enter / Alt+Enter dispatch their own actions
    /// that the editor resolves as a newline without the popup ever seeing them.
    /// Plain Enter here still runs `accept_line`, which takes the highlighted
    /// candidate — that path is untouched.
    fn insert_newline_action(&mut self, cx: &mut Context<Self>) {
        if !self.input_active() || self.reverse_search.is_some() {
            cx.propagate();
            return;
        }
        // A key pressed while scrolled up must edit the line the viewport is
        // showing, the way every editor key does (see `handle_editor_key`).
        self.jump_to_prompt();
        self.close_completion();
        self.cursor_visible = true;
        self.cmd.insert_str("\n");
        self.history_nav = None;
        // This action bypasses `handle_editor_key`, so it has to repeat that
        // dispatcher's per-key state resets itself — same reason `commit_text`
        // does for the IME path. Without them the next ↑/↓ takes its column
        // from a stale goal, and an ⌥. walk would continue across the newline.
        self.editor_goal_col = None;
        self.last_word_nav = None;
        cx.notify();
    }

    /// readline's accept-line, as the editor means it: with a completion
    /// candidate highlighted, take the candidate (a second stroke then runs the
    /// line); otherwise close any menu and submit. Shared by Enter and its
    /// control-code aliases ⌃J / ⌃M.
    fn accept_line(&mut self, cx: &mut Context<Self>) {
        if self
            .completion
            .as_ref()
            .is_some_and(|s| s.selected().is_some())
        {
            self.completion_accept(cx);
            return;
        }
        self.close_completion();
        self.submit_command(cx);
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
        // but always tally the run — count, the directory it ran in, and when —
        // for ranking and the Ctrl+R menu, then refresh the ranked view for the
        // current directory. The file record is deferred until the shell reports
        // back at its prompt, so it can carry this run's exit code; a previous
        // record still deferred goes out first.
        if !line.trim().is_empty() {
            let cwd = self.cwd();
            let now = unix_now();
            *self.history_counts.entry(line.clone()).or_insert(0) += 1;
            if let Some(dir) = cwd.as_ref().and_then(|p| p.to_str()) {
                self.history_cwds
                    .entry(line.clone())
                    .or_default()
                    .insert(dir.to_string());
            }
            self.history_meta.insert(
                line.clone(),
                super::history::EntryMeta {
                    ts: Some(now),
                    exit: None,
                },
            );
            if self.history.last().map(String::as_str) != Some(line.as_str()) {
                self.history.push(line.clone());
            }
            self.flush_pending_history();
            self.pending_history = Some(PendingHistory {
                line: line.clone(),
                cwd: cwd.clone(),
                ts: now,
                seq: self.terminal.prompt_seq(),
            });
            self.rerank_history(cwd.as_deref());
        }
        self.history_nav = None;
        self.history_stash.clear();
        self.close_completion();

        // Any gap typeahead still waiting for its wipe (the ^U is deferred
        // until zle reads) would prefix the submitted line on zle's side —
        // "ls" strays + "pwd\r" runs `lspwd`. Wipe first: FIFO puts the ^U
        // ahead of the line bytes.
        self.wipe_pending_typeahead();
        // One paste + one CR when the shell takes bracketed paste, so a
        // multi-line command costs one prompt cycle instead of one per line
        // (see `submit_bytes`); per-line CRs otherwise.
        let bracketed = self
            .terminal
            .term
            .lock()
            .mode()
            .contains(TermMode::BRACKETED_PASTE);
        self.terminal.write(submit_bytes(&line, bracketed));
        self.cmd.clear();
        self.cursor_visible = true;
        self.jump_to_prompt();
        cx.notify();
    }

    /// Readline's `yank-last-arg` (⌥.): drop the last word of the previous
    /// command at the caret. Repeating the chord walks further back through the
    /// history, each press swapping out the word the one before it inserted, so
    /// a run of presses leaves exactly one word behind. Entries with no words
    /// are stepped over rather than inserting nothing.
    fn insert_last_word(&mut self, cx: &mut Context<Self>) {
        // Only trust the recorded walk while the line still shows it: its word
        // sitting at `at`, caret at the word's end, nothing selected. The keys
        // this dispatcher sees reset `last_word_nav` themselves, but edits that
        // bypass it (IME-committed text, a paste, a completion pick, ⌘Z) don't
        // — resuming over those would delete text the walk never inserted.
        let resumed = self.last_word_nav.take().filter(|walk| {
            let len = walk.word.chars().count();
            self.cmd.cursor() == walk.at + len
                && self.cmd.selection().is_none()
                && self
                    .cmd
                    .text()
                    .chars()
                    .skip(walk.at)
                    .take(len)
                    .eq(walk.word.chars())
        });
        // A repeat resumes one entry older than the last press; a fresh walk
        // starts at the newest entry.
        let start = match &resumed {
            Some(walk) => walk.entry.checked_sub(1),
            None => self.history.len().checked_sub(1),
        };
        let Some(mut entry) = start else {
            // Nothing older to reach (or no history at all) — leave the line as
            // it stands, the word the previous press inserted included.
            self.last_word_nav = resumed;
            return;
        };
        let word = loop {
            if let Some(w) = self.history[entry].split_whitespace().next_back() {
                break w.to_string();
            }
            let Some(older) = entry.checked_sub(1) else {
                self.last_word_nav = resumed;
                return;
            };
            entry = older;
        };

        // Take back what the previous press left, so the walk swaps words in
        // place rather than piling them up.
        if let Some(walk) = resumed {
            self.cmd.clear_selection();
            self.cmd.set_cursor(walk.at);
            self.cmd.extend_to(walk.at + walk.word.chars().count());
            self.cmd.delete_selection();
        }
        self.cmd.insert_str(&word);
        // `insert_str` replaces a live selection first, which moves the caret
        // to the selection's start — so the word's position is wherever the
        // caret landed minus the word, not the pre-insert cursor.
        let at = self.cmd.cursor() - word.chars().count();
        self.last_word_nav = Some(LastWordWalk { entry, at, word });
        // The line is now the user's own edit, not a recalled entry.
        self.history_nav = None;
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
        let cwd_str = cwd.and_then(|p| p.to_str());
        self.history_ranked = super::history::rank_by_frecency(
            &self.history,
            &self.history_counts,
            &self.history_cwds,
            cwd_str,
        );
        self.history_frecency = super::history::frecency_scores(
            &self.history,
            &self.history_counts,
            &self.history_cwds,
            cwd_str,
        );
        self.ranked_cwd = cwd.map(std::path::Path::to_path_buf);
    }

    /// Write the deferred history record (see [`PendingHistory`]), if any. The
    /// exit code is attached only when the shell has reported back *and* sits
    /// at its prompt again — then `last_exit_code()` is this command's;
    /// otherwise (pane going away mid-command, a new submit racing in) the
    /// record goes out without one, like a plain shell history line.
    fn flush_pending_history(&mut self) {
        let Some(p) = self.pending_history.take() else {
            return;
        };
        let exit = (self.terminal.prompt_seq() > p.seq && self.terminal.at_prompt())
            .then(|| self.terminal.last_exit_code())
            .flatten();
        if exit.is_some()
            && let Some(m) = self.history_meta.get_mut(&p.line)
        {
            m.exit = exit;
        }
        super::history::append(&p.line, p.cwd.as_deref(), p.ts, exit);
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

    /// Raise the one-shot integration notice if this Ctrl+R fell through to the
    /// raw PTY path because shell integration never engaged (#46). Silent when
    /// the raw path is expected instead: integration did engage and a foreground
    /// command merely owns the PTY, a full-screen TUI owns the pane, or the
    /// shell is still inside its startup grace window (slow rc files haven't
    /// reached the first prompt report yet).
    ///
    /// Shows a generic message immediately, then refines it off-thread: the
    /// daemon's foreground query sees the process actually holding the PTY, and
    /// when that is a known shim (it exec'd over the shell we spawned), naming
    /// it turns "the feature looks broken" into "here is the culprit".
    fn note_integration_gap(&mut self, cx: &mut Context<Self>) {
        if self.integration_notice_shown
            || self.terminal.shell_active()
            || self.on_alt_screen()
            || self.created_at.elapsed() < INTEGRATION_GRACE
        {
            return;
        }
        self.integration_notice_shown = true;
        self.integration_notice = Some(integration_notice_message(None));
        cx.notify();

        let pane_id = self.pane_id;
        // This pane's own daemon: the id below is only meaningful there, and on
        // a remote workspace the local daemon would answer about a different
        // pane that happens to share the number.
        let route = self.pane_route();
        cx.spawn(async move |this, cx| {
            // Best-effort: no daemon / unknown pane / unreadable process just
            // leaves the generic message standing.
            let fg = cx
                .background_executor()
                .spawn(async move {
                    RemoteTerminal::list_panes_on(&route)
                        .into_iter()
                        .find(|p| p.pane_id == pane_id)
                        .map(|p| p.title)
                })
                .await;
            if let Some(shim) = fg.as_deref().and_then(known_pty_shim) {
                let _ = this.update(cx, |view, cx| {
                    if view.integration_notice.is_some() {
                        view.integration_notice = Some(integration_notice_message(Some(shim)));
                        cx.notify();
                    }
                });
            }
            // Reading time is over either way; a keystroke usually beat us here.
            cx.background_executor()
                .timer(INTEGRATION_NOTICE_TIMEOUT)
                .await;
            let _ = this.update(cx, |view, cx| {
                if view.integration_notice.take().is_some() {
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Begin a Ctrl+R history search (no-op if one is already active). Opens
    /// with the empty query's frecency listing, so the menu is browsable
    /// before a single key is typed.
    fn start_reverse_search(&mut self) {
        if self.reverse_search.is_none() {
            self.reverse_search = Some(ReverseSearch::new(&self.history, &self.history_frecency));
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
                        rs.push_query(ch, &self.history, &self.history_frecency);
                    }
                    cx.notify();
                    return;
                }
            }
        }
        let Some(rs) = self.reverse_search.as_mut() else {
            return;
        };
        match rs.handle_key(ks, &self.history, &self.history_frecency) {
            reverse_search::Action::Redraw => {}
            reverse_search::Action::Cancel => self.reverse_search = None,
            reverse_search::Action::Accept(line) => {
                self.reverse_search = None;
                if let Some(line) = line {
                    self.cmd.set(&line);
                }
            }
            reverse_search::Action::Run(line) => {
                // Cmd+Enter: accept the selection and run it in one stroke.
                self.reverse_search = None;
                self.cmd.set(&line);
                self.submit_command(cx);
            }
        }
        cx.notify();
    }

    /// Hand the prompt line over to the shell so *its* keymap answers a chord
    /// tty7 declines: ship the locally edited text to the PTY (no newline),
    /// clear the editor, send `chord`, and suspend the local editor until the
    /// shell's next report. From here the shell's own editor holds the text —
    /// re-engaging ours mid-line would fork the two buffers (its Enter would
    /// submit an empty local line on top of zle's populated one).
    ///
    /// A multi-line draft can't make the trip (see below): the chord is
    /// swallowed and the line stays local.
    fn handoff_line_to_shell(&mut self, chord: &[u8], cx: &mut Context<Self>) {
        // Fold in any gap input still held, so the shipped line is what the
        // user actually typed.
        if let Some(net) = self.hold.engage() {
            self.cmd.prepend_str(&net);
        }
        let line = self.cmd.text();
        // An embedded newline would submit on the shell side (zle runs the
        // line on `\r`), so a multi-line draft can't be handed over losslessly
        // — keep it local and swallow the chord as before.
        if line.contains('\n') {
            cx.notify();
            return;
        }
        self.close_completion();
        // A pending typeahead wipe's deferred `^U` would erase the very text
        // we're about to ship; flush it first (FIFO keeps it ahead).
        self.wipe_pending_typeahead();
        // Chars right of the caret: after the shipped text lands, walk zle's
        // cursor back over them so the shell acts on the word the caret was
        // on, not the line's tail.
        let tail = line.chars().count().saturating_sub(self.cmd.cursor());
        if !line.is_empty() {
            self.terminal.write(line.into_bytes());
            if tail > 0 {
                self.terminal.write(b"\x1b[D".repeat(tail));
            }
        }
        self.cmd.clear();
        self.editor_handoff = Some(self.terminal.prompt_cycle());
        self.send_to_pty(chord, cx);
    }

    /// Tab / Shift-Tab arrived. Either our completion answers it, or the raw
    /// key goes to the shell.
    ///
    /// One entry point for both directions so the "why didn't the menu open"
    /// trace has one place to live — this is the question every report about
    /// completion turns out to be.
    fn tab_pressed(&mut self, forward: bool, cx: &mut Context<Self>) {
        if self.search_focused {
            cx.propagate();
            return;
        }
        if let Some(reason) = self.input_inactive_reason() {
            log::debug!(target: "tty7::completion", "Tab goes straight to the PTY: {reason}");
            let bytes = self.tab_bytes(!forward);
            self.send_to_pty(&bytes, cx);
            return;
        }
        self.complete_tab(forward, cx);
    }

    /// Hand the line over and let the shell have the Tab, so its native
    /// completion (compsys, fzf-tab, …) answers what tty7 has nothing for.
    fn handoff_tab_to_shell(&mut self, shift: bool, cx: &mut Context<Self>) {
        let bytes = self.tab_bytes(shift);
        self.handoff_line_to_shell(&bytes, cx);
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
        // Ctrl+R search owns the keyboard: `self.cmd` still holds the stale
        // pre-search line, so neither completing it nor shipping it to the
        // shell makes sense here.
        if self.reverse_search.is_some() {
            return;
        }
        // tty7 completion switched off: every Tab goes to the shell.
        if !cx.global::<Config>().tab_completion {
            log::debug!(target: "tty7::completion", "handing the line to the shell: tab_completion is off");
            self.handoff_tab_to_shell(!forward, cx);
            return;
        }
        if self.completion.is_some() {
            self.completion_tab_step(forward, cx);
            return;
        }

        // Fresh completion. Path candidates come off the local filesystem, so
        // they need a local cwd — a remote pane passes `None` and gets command
        // completion only. Falling back to tty7's own directory there would
        // offer *this* machine's filenames for insertion into a remote command
        // line, where they don't exist.
        //
        // The `current_dir` fallback is for a *local* pane whose shell has not
        // reported OSC 7 yet, and must stay behind the same gate: reaching it
        // from a remote pane is the same wrong answer by a longer route.
        let cwd = self
            .paths_are_local()
            .then(|| self.local_cwd().or_else(|| std::env::current_dir().ok()))
            .flatten();
        let line = self.cmd.text();
        let cursor = self.cmd.cursor();
        let Some(comp) = super::completion::complete(&line, cursor, cwd.as_deref()) else {
            // Nothing *locally*. A native-SSH pane can still answer for the
            // remote filesystem over its own connection — ask before giving up
            // the line (see `spawn_remote_path_completion`).
            if self.spawn_remote_path_completion(&line, cursor, forward, cx) {
                return;
            }
            // Nothing to offer. Don't swallow the keypress (#136) — hand the
            // line to the shell and let its completion have the Tab.
            log::debug!(
                target: "tty7::completion",
                "handing the line to the shell: no candidates for {line:?} at {cursor} \
                 (local cwd {cwd:?}, remote cwd {:?})",
                self.remote_ssh_cwd(),
            );
            self.handoff_tab_to_shell(!forward, cx);
            return;
        };

        // With generators inbound the candidate set is still growing, so the
        // usual "unique sync match → accept" and "fill the common prefix"
        // shortcuts are unsafe: a result landing a moment later could add or
        // change the pick. Only the fully-static case (no pending) keeps the
        // classic behavior byte-for-byte.
        let has_pending = !comp.pending.is_empty();

        // The word range is carried by any candidate; with none (pure-generator
        // slot) derive it from the caret so the session still knows what it
        // replaces.
        let (word_start, word_end) = match comp.candidates.first() {
            Some(c) => (c.start, c.end),
            None => (word_start_of(&line, cursor), cursor),
        };
        let Some(generation) = self.offer_candidates(
            &line,
            word_start,
            word_end,
            comp.candidates,
            has_pending,
            cx,
        ) else {
            // A unique match was accepted outright; nothing is open to merge into,
            // and a static-only slot has no generators anyway.
            return;
        };

        // Kick off each generator on the background executor and merge results
        // back on the main thread, tagged with this session's generation.
        // Generators are local shell-outs and only ever come from `complete`'s
        // `Some(cwd)` branch, so a remote pane has none to run.
        let Some(cwd) = cwd else { return };
        for pending in comp.pending {
            let script = pending.script;
            let cwd = cwd.clone();
            cx.spawn(async move |this, cx| {
                let results = cx
                    .background_executor()
                    .spawn(async move { super::generator::run(&script, &cwd) })
                    .await;
                if results.is_empty() {
                    return;
                }
                let _ = this.update(cx, |view, cx| {
                    view.completion_merge(generation, results, cx);
                });
            })
            .detach();
        }
    }

    /// Put `cands` in front of the user: accept a unique match outright, else
    /// open the menu over them (filling the longest common prefix first).
    /// Returns the opened session's generation, or `None` when a unique match
    /// was accepted and no menu exists.
    ///
    /// `has_pending` means more candidates are still inbound, which disables
    /// both shortcuts: a result landing a moment later could add to or change
    /// the pick, and mutating the line before then would be jarring.
    fn offer_candidates(
        &mut self,
        line: &str,
        word_start: usize,
        word_end: usize,
        cands: Vec<completion::Candidate>,
        has_pending: bool,
        cx: &mut Context<Self>,
    ) -> Option<u64> {
        if !has_pending && cands.len() == 1 {
            let c = cands[0].clone();
            self.completion_insert(&c, c.start);
            self.cursor_visible = true;
            cx.notify();
            return None;
        }
        let word: String = line
            .chars()
            .skip(word_start)
            .take(word_end - word_start)
            .collect();
        let s = CompletionSession::new(word_start, word.clone(), cands);
        if !has_pending
            && let Some(lcp) = s.common_prefix()
            && lcp.chars().count() > word.chars().count()
        {
            // Fill the longest common prefix when it extends the typed word.
            // All candidates share it, so the fill never invalidates the set.
            self.apply_candidate(line, word_start, word_end, &lcp);
        }
        let generation = self.open_completion(s);
        self.cursor_visible = true;
        cx.notify();
        Some(generation)
    }

    /// The pane's cwd as a path on the *remote*, for panes whose filesystem
    /// tty7 can ask about over a connection it holds. `None` for every other
    /// kind, including a plain local pane.
    ///
    /// Two shapes qualify, and they are found by different signals:
    ///   - a **native-SSH pane** — tty7 dialled it, so `remote_context` says so
    ///     and the daemon holds the authenticated connection under its pane id;
    ///   - a **remote-workspace pane** — its `tty7-server` reports it as
    ///     an ordinary local pane (it *is* one, over there), so `remote_context`
    ///     is `None` and only this side's `workspace` binding reveals it. The
    ///     connection is the workspace's, not the pane's.
    ///
    /// Everything else declines rather than pretend: a foreground `ssh` and WSL
    /// have no tty7-owned connection to ask (WSL falls out of
    /// [`RemoteTerminal::workspace_request`], which needs an SSH spec), and a
    /// local pane is the local engine's business.
    fn remote_ssh_cwd(&self) -> Option<String> {
        let owned = match self.terminal.remote_context() {
            Some(remote) => remote.kind == crate::daemon::protocol::RemoteKind::NativeSsh,
            None => self.workspace.is_some(),
        };
        if !owned {
            return None;
        }
        let cwd = self.cwd()?.to_string_lossy().into_owned();
        cwd.starts_with('/').then_some(cwd)
    }

    /// Complete a path against the *remote* filesystem, over the pane's own SSH
    /// connection. Returns whether a request went out — the caller then leaves
    /// the Tab to us instead of handing the line to the shell.
    ///
    /// The listing is a daemon round-trip (`SftpList` on the pane's existing
    /// authenticated connection — the same channel the SFTP panel browses
    /// with), so it cannot answer this keystroke synchronously. Results land on
    /// the main thread and only *then* behave as a local Tab would; see
    /// [`Self::remote_path_results`] for what happens to a stale or empty one.
    ///
    /// Out-of-band deliberately. The other way to read a remote directory is to
    /// inject a listing command into the live shell and scrape it back out of
    /// the PTY — the only option for a terminal that merely *bootstrapped into*
    /// a session someone else dialled. tty7 opened this connection itself, so it
    /// can just ask: nothing is echoed into the scrollback, no prompt hooks need
    /// suppressing, and there's no stray background process to cancel when the
    /// user hits Enter. The in-band route stays the answer for the pane kinds
    /// that have no tty7-owned connection (a foreground `ssh`, WSL), which is
    /// why those decline in [`Self::remote_ssh_cwd`] rather than pretend.
    fn spawn_remote_path_completion(
        &mut self,
        line: &str,
        cursor: usize,
        forward: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(cwd) = self.remote_ssh_cwd() else {
            return false;
        };
        let Some(req) = completion::remote_path_request(line, cursor, &cwd) else {
            log::debug!(
                target: "tty7::completion",
                "no remote listing to ask for: {line:?} at {cursor} against {cwd}"
            );
            return false;
        };
        // A listing for this same keystroke is already on the wire. Swallow the
        // repeat rather than dialling again — the answer is about to arrive and
        // will open the menu.
        if self.remote_completion_inflight {
            return true;
        }
        self.remote_completion_inflight = true;
        // Resolved here, on the UI thread: the background call cannot reach the
        // pane entity, and a remote-workspace pane's listing has to go out on
        // the *workspace's* connection — its pane id names a pane on the far
        // daemon, which this one cannot resolve.
        let route = crate::ui::sftp::SftpRoute::new(self.pane_id, self.workspace.clone());
        let dir = req.dir.clone();
        let line = line.to_string();
        log::debug!(target: "tty7::completion", "listing {dir} over the remote's own connection");
        cx.spawn(async move |this, cx| {
            let listed = cx.background_spawn(async move { route.list(&dir) }).await;
            if let Err(e) = &listed {
                log::warn!(target: "tty7::completion", "remote listing failed: {e}");
            }
            let _ = this.update(cx, |view, cx| {
                view.remote_completion_inflight = false;
                view.remote_path_results(
                    req,
                    &line,
                    cursor,
                    listed.unwrap_or_default(),
                    forward,
                    cx,
                )
            });
        })
        .detach();
        true
    }

    /// Land a remote directory listing as completion candidates.
    ///
    /// Three outcomes, in the order they're checked:
    ///   - **the line moved on** while the network answered: drop it. The
    ///     answer describes a word the user is no longer typing, and the Tab
    ///     that asked for it is long past.
    ///   - **nothing matched**: fall back to the shell handoff, exactly as a
    ///     local no-match does (#136). A directory we couldn't read, or a
    ///     prefix with no entries, is then no worse off than before this
    ///     existed — the remote's own completion still gets its shot.
    ///   - **candidates**: offer them like any local Tab.
    fn remote_path_results(
        &mut self,
        req: completion::RemotePathRequest,
        line: &str,
        cursor: usize,
        listed: Vec<crate::daemon::protocol::SftpEntry>,
        forward: bool,
        cx: &mut Context<Self>,
    ) {
        if self.cmd.text() != line || self.cmd.cursor() != cursor {
            log::debug!(
                target: "tty7::completion",
                "dropping a remote listing for {line:?}: the line has moved on"
            );
            return;
        }
        let entries: Vec<completion::RemoteEntry> = listed
            .into_iter()
            .map(|e| completion::RemoteEntry {
                // Follow symlinks when classifying, as the local path engine
                // does: a link to a directory takes the trailing `/` and
                // survives a dirs-only filter (`cd` into one is routine). The
                // daemon resolved the target for us.
                is_dir: e.kind == crate::daemon::protocol::SftpEntryKind::Dir || e.target_is_dir,
                name: e.name,
            })
            .collect();
        let cands = completion::remote_path_candidates(&req, &entries);
        log::debug!(
            target: "tty7::completion",
            "{} entries in {}, {} match the word",
            entries.len(),
            req.dir,
            cands.len()
        );
        if cands.is_empty() {
            self.handoff_tab_to_shell(!forward, cx);
            return;
        }
        self.offer_candidates(line, req.word_start, req.cursor, cands, false, cx);
    }

    /// Open a completion menu and bump the generation tag, returning it so a
    /// caller spawning generators can stamp their in-flight results. Every open
    /// gets a fresh generation, so a slow generator from a prior session can't be
    /// mistaken for one belonging to this menu.
    fn open_completion(&mut self, session: CompletionSession) -> u64 {
        self.completion = Some(session);
        self.completion_generation = self.completion_generation.wrapping_add(1);
        self.completion_generation
    }

    /// Close the menu, bumping the generation so any generator still running for
    /// it is orphaned — its result will be dropped on arrival. No-op when nothing
    /// is open.
    fn close_completion(&mut self) {
        let _ = self.take_completion();
    }

    /// Take the open session (for accept), bumping the generation like
    /// [`Self::close_completion`].
    fn take_completion(&mut self) -> Option<CompletionSession> {
        let s = self.completion.take();
        if s.is_some() {
            self.completion_generation = self.completion_generation.wrapping_add(1);
        }
        s
    }

    /// Merge a finished generator's candidates into the open menu. Dropped unless
    /// the session that spawned it is still the current one (`generation` match) —
    /// the guard against a result outliving its menu. Rebuilds the candidate set
    /// against the *live* word (the caret may have moved on while the generator
    /// ran) and repaints.
    fn completion_merge(
        &mut self,
        generation: u64,
        results: Vec<super::generator::Parsed>,
        cx: &mut Context<Self>,
    ) {
        if self.completion_generation != generation || self.completion.is_none() {
            return;
        }
        let word_start = self.completion.as_ref().map(|s| s.word_start).unwrap_or(0);
        let chars: Vec<char> = self.cmd.text().chars().collect();
        let cursor = self.cmd.cursor().min(chars.len());
        let end = cursor.max(word_start);
        let live_word: String = if cursor >= word_start {
            chars[word_start..cursor].iter().collect()
        } else {
            String::new()
        };
        let new: Vec<completion::Candidate> = results
            .into_iter()
            .map(|p| completion::Candidate {
                text: p.text,
                kind: CandidateKind::Value,
                start: word_start,
                end,
                description: p.description,
                icon: None,
            })
            .collect();
        if let Some(s) = self.completion.as_mut() {
            s.merge(new, &live_word);
            cx.notify();
        }
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
        let Some(s) = self.take_completion() else {
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
            self.close_completion();
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
        if self.terminal.exited || text.is_empty() || !self.accepts_input(cx) {
            return;
        }
        // While reverse-searching, typed text edits the query, not the line.
        if let Some(rs) = self.reverse_search.as_mut() {
            rs.push_query(text, &self.history, &self.history_frecency);
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
            self.editor_goal_col = None;
            // Typed text ends an ⌥. run: IME-committed text bypasses
            // `handle_editor_key`'s reset, so it has to happen here too.
            self.last_word_nav = None;
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
        self.jump_to_prompt();
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
        shift: bool,
        cx: &mut Context<Self>,
    ) {
        let smart = cx.global::<Config>().smart_select;
        let mut term = self.terminal.term.lock();
        let display_offset = term.grid().display_offset() as i32;
        let point = Point::new(Line(row as i32 - display_offset), Column(col));
        let side = if left { Side::Left } else { Side::Right };
        // Shift+click extends the existing selection to the click instead of
        // starting over (à la iTerm2). A plain click always leaves a
        // collapsed Simple selection behind, so the anchor is wherever the
        // last gesture ended.
        if shift && clicks == 1 && term.selection.is_some() {
            if let Some(sel) = term.selection.as_mut() {
                sel.update(point, side);
            }
            drop(term);
            self.selecting = true;
            cx.notify();
            return;
        }
        let ty = match clicks {
            2 => SelectionType::Semantic, // word
            n if n >= 3 => SelectionType::Lines,
            _ => SelectionType::Simple,
        };
        let mut selection = Selection::new(ty, point, side);
        // Double-click smart selection: a URL / path / email / bracket pair /
        // CJK word containing the clicked word replaces the plain word span.
        // Boundary-flanked candidates anchor a Semantic selection (keeping
        // the drag gesture word-wise); exact ones use Simple so alacritty
        // can't re-expand the endpoints past the smart boundary.
        if clicks == 2
            && smart
            && let Some(r) = super::smart_select::grid_smart_range(&term, point)
        {
            let ty = if r.exact {
                SelectionType::Simple
            } else {
                SelectionType::Semantic
            };
            selection = Selection::new(ty, r.start, Side::Left);
            selection.update(r.end, Side::Right);
        }
        term.selection = Some(selection);
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

    /// Mouse-up: the selection gesture (if any) is over. With copy-on-select
    /// enabled, the selection the gesture drove goes straight to the clipboard
    /// — [`select_end_copy`] picks the buffer, and empty selections (a plain
    /// click repositioning the caret / collapsing the old selection) write
    /// nothing because both copy paths drop empty text.
    pub fn on_select_end(&mut self, cx: &mut Context<Self>) {
        let copy = select_end_copy(
            cx.global::<Config>().copy_on_select,
            self.selecting,
            self.editor_select_gesture,
        );
        self.selecting = false;
        self.editor_selecting = false;
        self.editor_select_gesture = false;
        self.editor_drag_word = None;
        self.drag_scroll = None;
        match copy {
            SelectEndCopy::None => {}
            SelectEndCopy::Grid => self.copy_selection(cx),
            SelectEndCopy::Editor => {
                if let Some(text) = self.cmd.selected_text() {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
            }
        }
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

    /// The pane's OSC 133 command marks, newest last — the Outline's rows.
    pub fn command_marks(&self) -> Vec<crate::terminal::marks::CommandMark> {
        self.terminal.marks().list()
    }

    /// Scroll so the command recorded at `row` sits near the top of the viewport.
    /// Returns `false` when the mark has aged out of the scrollback, so the
    /// caller can say so rather than leaving the user staring at an unchanged
    /// screen wondering whether the click registered.
    ///
    /// `row` is an index from the top of history, which drifts once the
    /// scrollback saturates (see the `terminal::marks` docs). A drifted mark
    /// still scrolls *somewhere* — it just may not be the exact prompt — so the
    /// only failure reported here is a row that has fallen off entirely.
    pub fn scroll_to_mark(&mut self, row: i64, cx: &mut Context<Self>) -> bool {
        use alacritty_terminal::grid::Dimensions as _;
        let mut term = self.terminal.term.lock();
        let history = term.grid().history_size() as i64;
        if row < 0 || row > history + term.grid().screen_lines() as i64 {
            return false;
        }
        // `display_offset` counts *up* from the bottom of history, so the offset
        // that puts `row` at the viewport's top line is its distance from there.
        let target = (history - row).max(0);
        let current = term.grid().display_offset() as i64;
        term.scroll_display(Scroll::Delta((target - current) as i32));
        drop(term);
        // A jump lands wherever it lands; the fractional offset is a smooth-scroll
        // artifact and would otherwise shift the paint off the line boundary.
        self.scroll_frac = 0.;
        cx.notify();
        true
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

    /// The grid line a screen `row` currently maps to, or `None` when that row
    /// is outside the grid.
    ///
    /// Mandatory before indexing: `Grid`'s `Index<Line>` only `debug_assert`s
    /// the bound, so a release build walks off the storage and panics on the
    /// slice check instead. A remembered cell goes stale whenever the grid
    /// shrinks under it — split a pane, drag the window smaller — and the
    /// callers here run inside gpui's `extern "C"` input callbacks, where that
    /// panic can't unwind and aborts the process.
    fn grid_line(
        term: &alacritty_terminal::Term<crate::terminal::remote::EventProxy>,
        row: usize,
    ) -> Option<Line> {
        let line = Line(row as i32 - term.grid().display_offset() as i32);
        (line >= term.topmost_line() && line <= term.bottommost_line()).then_some(line)
    }

    /// Open the link under the given cell, if any (OSC 8 hyperlink, plain URL or
    /// existing file or directory path detected in the row text). Returns true if one opened.
    pub fn open_link_at(
        &self,
        col: usize,
        row: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !cx.global::<Config>().link_url {
            return false;
        }
        let include_loopback = self.can_forward_loopback(cx);
        let Some((target, _start, _end)) = self.resolve_link_at(col, row, true, include_loopback)
        else {
            return false;
        };
        match target {
            LinkTarget::Url(url) => self.open_url(&url, window, cx),
            LinkTarget::File { path, line, column } => {
                // A configured template (e.g. opening the file in an editor)
                // takes precedence; otherwise fall back to the OS opener.
                match cx.global::<Config>().link_file_command.as_deref() {
                    Some(template) => run_file_command(template, &path, line, column),
                    None => open_file_path(&path),
                }
            }
        }
        true
    }

    fn open_url(&self, url: &str, window: &mut Window, cx: &mut Context<Self>) {
        match self.forwarded_loopback_url(url, cx) {
            LoopbackOpen::Forwarded(url) => cx.open_url(&url),
            LoopbackOpen::NotLoopback => cx.open_url(url),
            // The click produced no browser tab, so it has to say why: silence
            // here reads as tty7 having ignored the click.
            LoopbackOpen::ForwardFailed(reason) => {
                window.push_notification(reason, cx);
            }
        }
    }

    fn forwarded_loopback_url(&self, url: &str, cx: &mut Context<Self>) -> LoopbackOpen {
        let plan = self.loopback_plan(cx);
        if matches!(plan, LoopbackPlan::Direct) {
            return LoopbackOpen::NotLoopback;
        }
        let Some(loopback) = super::loopback::parse_loopback_url(url) else {
            return LoopbackOpen::NotLoopback;
        };
        // WSL shares the Windows host's `localhost`, so the URL already points at
        // the right place — building a forward would be pure overhead.
        if matches!(plan, LoopbackPlan::NoForwardNeeded) {
            return LoopbackOpen::NotLoopback;
        }

        // Establishing a local forward is a `bind()` on this machine plus a
        // registry insert — no SSH round-trip happens until the browser actually
        // connects — so this stays a blocking call rather than paying for an
        // async hop the user would perceive as the click doing nothing.
        //
        // The local port is always ephemeral (`bind_port: 0`, chosen by the OS):
        // the remote's 3000 may well be taken here, and the URL is rewritten to
        // whichever port we actually got.
        let forwarded = match &plan {
            LoopbackPlan::ForwardOnPane(pane_id) => RemoteTerminal::ensure_loopback_forward(
                *pane_id,
                loopback.forward_host(),
                loopback.port,
            ),
            LoopbackPlan::ForwardOnWorkspace(ws) => self.ensure_workspace_loopback(ws, &loopback),
            LoopbackPlan::Direct | LoopbackPlan::NoForwardNeeded => unreachable!("handled above"),
        };
        match forwarded {
            Ok(forward) => LoopbackOpen::Forwarded(loopback.forwarded_url(forward.local_port)),
            Err(e) => {
                log::warn!("failed to forward loopback URL {url}: {e}");
                LoopbackOpen::ForwardFailed(format!("Couldn't forward :{} — {e}", loopback.port))
            }
        }
    }

    /// Ask the daemon for a workspace-owned forward to `loopback`'s port.
    fn ensure_workspace_loopback(
        &self,
        ws: &crate::terminal::PaneWorkspace,
        loopback: &super::loopback::LoopbackUrl,
    ) -> anyhow::Result<crate::daemon::protocol::LoopbackForward> {
        let req = RemoteTerminal::workspace_request(
            ws,
            self.pane_id,
            crate::daemon::protocol::WorkspaceOp::EnsureLoopback {
                remote_host: loopback.forward_host().to_string(),
                remote_port: loopback.port,
            },
        )
        .ok_or_else(|| anyhow::anyhow!("this workspace has no SSH connection to forward over"))?;
        match RemoteTerminal::on_workspace(req)? {
            crate::daemon::protocol::DaemonMsg::LoopbackForward(f) => Ok(f),
            other => Err(anyhow::anyhow!("unexpected reply: {other:?}")),
        }
    }

    /// Decide how a ⌘-clicked `localhost:PORT` in *this* pane should be opened.
    fn loopback_plan(&self, cx: &mut Context<Self>) -> LoopbackPlan {
        loopback_plan(
            cx.global::<Config>().ssh_loopback_forward,
            self.workspace.as_ref(),
            self.terminal.remote_context().map(|r| r.kind),
            self.pane_id,
        )
    }

    fn can_forward_loopback(&self, cx: &mut Context<Self>) -> bool {
        !matches!(self.loopback_plan(cx), LoopbackPlan::Direct)
    }

    /// Update the remembered hovered link for the screen cell `(col, row)` and
    /// repaint if it changed. Returns whether a link sits under the cursor, so the
    /// element can switch to a pointing-hand cursor. Cheap on the common case: any
    /// non-URL cell resolves to `None` and bails.
    pub fn hover_link_at(
        &mut self,
        col: usize,
        row: usize,
        include_files: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        self.last_hover_cell = Some((col, row));
        // URL detection off → never underline or switch to the pointing hand,
        // and drop any underline a prior hover left behind.
        if !cx.global::<Config>().link_url {
            self.clear_hovered_link(cx);
            return false;
        }
        let include_loopback = self.can_forward_loopback(cx);
        let next = self.link_span_at(col, row, include_files, include_loopback);
        if next != self.hovered_link {
            self.hovered_link = next;
            cx.notify();
        }
        self.hovered_link.is_some()
    }

    pub fn refresh_link_hover(&mut self, include_files: bool, cx: &mut Context<Self>) -> bool {
        self.link_modifier_down = include_files;
        let Some((col, row)) = self.last_hover_cell else {
            return false;
        };
        self.hover_link_at(col, row, include_files, cx)
    }

    pub fn link_modifier_down(&self) -> bool {
        self.link_modifier_down
    }

    /// Forget any hovered link (mouse left the grid, or moved onto plain text),
    /// repainting to drop the underline.
    pub fn clear_hovered_link(&mut self, cx: &mut Context<Self>) {
        self.last_hover_cell = None;
        if self.hovered_link.take().is_some() {
            cx.notify();
        }
    }

    /// Resolve the link span at screen cell `(col, row)`: an OSC 8 hyperlink (the
    /// contiguous run of cells sharing the same target), a bare URL token, or an
    /// existing file or directory path in the row text. Mirrors [`open_link_at`](Self::open_link_at)'s
    /// detection so the underline covers exactly what a Cmd+click would open.
    fn link_span_at(
        &self,
        col: usize,
        row: usize,
        include_files: bool,
        include_loopback: bool,
    ) -> Option<HoveredLink> {
        self.resolve_link_at(col, row, include_files, include_loopback)
            .map(|(_, start, end)| HoveredLink { start, end })
    }

    /// The link under screen cell `(col, row)` and the inclusive grid points it
    /// spans, shared by hover-underline and click-to-open so both agree on the
    /// extent. Resolution runs over the *logical* line — soft-wrapped rows plus
    /// producer hard newlines are stitched back together — so a URL split across
    /// rows resolves whole instead of stopping at the first row edge.
    fn resolve_link_at(
        &self,
        col: usize,
        row: usize,
        include_files: bool,
        include_loopback: bool,
    ) -> Option<(LinkTarget, Point, Point)> {
        let term = self.terminal.term.lock();
        let line = Self::grid_line(&term, row)?;
        let cols = term.columns();
        if col >= cols {
            return None;
        }
        let click = Point::new(line, Column(col));

        // 1) Explicit OSC 8 hyperlink: highlight the whole contiguous run
        //    carrying the same URI, following soft wraps across rows.
        if let Some(hl) = term.grid()[line][Column(col)].hyperlink() {
            let uri = hl.uri().to_string();
            if let Some((start, end)) = super::smart_select::hyperlink_run(&term, click) {
                return Some((LinkTarget::Url(uri), start, end));
            }
        }

        // 2) Bare URL or file path detected in the logical line. `bridge_hard_wrap`
        //    is on so a URL a program printed with a literal `\n` mid-way is
        //    recovered whole, not truncated at the break.
        let (text, points, click_idx) = super::smart_select::logical_line_at(&term, click, true)?;
        drop(term);
        // Same gate as the click path — a relative path is resolved against the
        // cwd and stat-checked, so a remote pane's cwd must not be used.
        let cwd = self.local_cwd();
        let link = super::search::link_at(&text, click_idx, cwd.as_deref(), include_files)
            .or_else(|| {
                include_loopback.then(|| {
                    super::loopback::loopback_url_span_at(&text, click_idx).map(
                        |(start, end, url)| super::search::LinkMatch {
                            start,
                            end,
                            target: LinkTarget::Url(url),
                        },
                    )
                })?
            })?;
        Some((link.target, points[link.start], points[link.end]))
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
        let cx_left = px(GRID_PAD_X) + self.cell_width * (ccol as f32);
        // The overlay rides the same upward shift `element::paint` applies to
        // the grid when the wrapped input would spill past the bottom, so its
        // first line keeps hugging the (shifted) prompt row. May go negative
        // when the input is taller than the screen — the parent's
        // `overflow_hidden` clips the rows that scroll off the top.
        let shift = self.input_scroll_rows();
        let cy_top = px(GRID_PAD_Y) + self.line_height * (crow as f32 - shift as f32);

        // Reverse-search mode replaces the line with a `(reverse-i-search)` prompt
        // showing the query and the selected match; the ranked candidates float
        // in their own menu (`render_reverse_search_menu`).
        if let Some(rs) = &self.reverse_search {
            let label = format!("(reverse-i-search)`{}': ", rs.query());
            let matched = rs
                .selected_line(&self.history)
                .unwrap_or_default()
                .to_string();
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

        // A blank cell of the given width and the line height — used for the
        // leading prompt spacer and for a selected/caret slot standing in for a
        // hard line break.
        let blank = move |w: gpui::Pixels| div().flex_none().w(w).h(lh);

        // The buffer's logical lines, each rendered as its own `flex_wrap` row and
        // stacked in a column, so an embedded `'\n'` (from a pasted multi-line
        // command, or Shift/Opt+Enter) shows as a real line break instead of
        // flowing into one wrapped blob. Within a line, soft-wrapping is left to
        // `flex_wrap` exactly as before. `lines` grows a fresh row on each `'\n'`.
        let mut lines: Vec<Vec<gpui::AnyElement>> = vec![vec![
            // Leading spacer the width of the shell prompt: the first line begins
            // right after the prompt; continuation lines start at the grid's left
            // edge, matching how the shell lays a multi-line command out.
            blank(cell_w * (ccol as f32)).into_any_element(),
        ]];

        // Ghost suggestion only makes sense for a single-line command (it completes
        // the whole history entry); suppress it once the buffer holds a newline.
        let is_multiline = chars.contains(&'\n');

        for i in 0..len {
            // IME pre-edit shows underlined at the caret; the bar caret is hidden
            // while composing.
            if i == cursor && has_marked {
                for mc in marked.chars() {
                    lines
                        .last_mut()
                        .unwrap()
                        .push(cell(fg, mc, false, false, true));
                }
            }
            if chars[i] == '\n' {
                // The newline is a hard break, not a glyph. If the caret sits on it
                // (end of this visual line) draw a trailing caret slot before the
                // break so it stays visible; if the newline falls inside a selection
                // draw a thin selected slot so a multi-line selection reads across
                // the break. Then start the next row.
                if selection.is_none() && !has_marked && cursor_on && cursor == i {
                    lines.last_mut().unwrap().push(
                        blank(cell_w)
                            .relative()
                            .child(caret_bar())
                            .into_any_element(),
                    );
                } else if selection.is_some_and(|(s, e)| i >= s && i < e) {
                    lines
                        .last_mut()
                        .unwrap()
                        .push(blank(cell_w).bg(sel_bg).into_any_element());
                }
                lines.push(Vec::new());
                continue;
            }
            let selected = selection.is_some_and(|(s, e)| i >= s && i < e);
            let caret = selection.is_none() && !has_marked && cursor_on && cursor == i;
            lines
                .last_mut()
                .unwrap()
                .push(cell(colors[i], chars[i], selected, caret, false));
        }

        // Ghost autosuggestion remainder (only when caret is at the end, no
        // selection / IME / newline), computed up front so the end-of-line caret can
        // ride on the first ghost cell instead of needing its own (which would push
        // the ghost a full cell to the right).
        let ghost: Option<String> = if selection.is_none() && !has_marked && !is_multiline {
            self.ghost_suggestion()
                .map(|full| full.chars().skip(len).collect::<String>())
                .filter(|r| !r.is_empty())
        } else {
            None
        };

        // Caret / pre-edit at the end of the buffer — lands on the last row (a
        // fresh empty row when the buffer ends in a newline).
        if cursor == len {
            let last = lines.last_mut().unwrap();
            if has_marked {
                for mc in marked.chars() {
                    last.push(cell(fg, mc, false, false, true));
                }
            } else if ghost.is_none() {
                // No ghost following: a trailing cell carries the caret (and is the
                // click target for "end of line").
                let mut tail = blank(cell_w).relative();
                if selection.is_none() && cursor_on {
                    tail = tail.child(caret_bar());
                }
                last.push(tail.into_any_element());
            }
            // else: the caret rides on the first ghost cell below.
        }

        if let Some(rem) = ghost {
            let last = lines.last_mut().unwrap();
            for (gi, gc) in rem.chars().enumerate() {
                let caret = gi == 0 && cursor == len && cursor_on;
                last.push(cell(muted, gc, false, caret, false));
            }
        }

        let rows = lines.into_iter().map(move |cells| {
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .w_full()
                .min_h(lh)
                .children(cells)
        });

        div()
            .absolute()
            .left(px(GRID_PAD_X))
            .top(cy_top)
            .right_4()
            .min_h(lh)
            .flex()
            .flex_col()
            // Transparent: the text overlays the grid in place, reading as a
            // natural continuation of the shell prompt rather than a separate bar.
            .font_family(self.font.family.clone())
            .text_size(self.font_size)
            .line_height(lh)
            .text_color(fg)
            .children(rows)
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
        // Anchor rows are *visual*: when the overflowing input shifts the whole
        // surface up (`input_scroll_rows`), the menu must follow the shifted
        // input row, not the unshifted grid row.
        let srow = srow.saturating_sub(self.input_scroll_rows());

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
        let x = px(GRID_PAD_X) + self.cell_width * (scol as f32);
        let y = if place_above {
            px(GRID_PAD_Y) + self.line_height * (srow as f32) - menu_h - gap
        } else {
            px(GRID_PAD_Y) + self.line_height * ((srow + 1) as f32) + gap
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

    /// The floating Ctrl+R history menu: the ranked matches (best first) in a
    /// completion-style popup anchored to the input row — matched characters
    /// highlighted, the last-run time and a failure badge on the right. The
    /// classic `(reverse-i-search)` prompt stays on the input row itself
    /// (`render_input_bar`); this menu is the browsable view of the candidates,
    /// windowed around the selection like the completion menu.
    fn render_reverse_search_menu(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement + use<>> {
        let rs = self.reverse_search.as_ref()?;
        let matches = rs.matches();
        if matches.is_empty() {
            return None;
        }
        let (srow, _) = self.cursor_cell()?;

        const MAX_ROWS: usize = 10;
        let (total_rows, total_cols) = {
            let term = self.terminal.term.lock();
            (term.screen_lines(), term.columns())
        };
        let (place_above, visible, first) =
            menu_layout(total_rows, srow, matches.len(), rs.selected(), MAX_ROWS);
        let hidden_above = first;
        let hidden_below = matches.len() - first - visible;

        let theme = cx.theme();
        let lh = self.line_height;
        let now = unix_now();
        let row = |i: usize| {
            let m = &matches[i];
            let line = self.history[m.index].as_str();
            let selected = rs.selected() == i;
            let base = if selected {
                theme.foreground
            } else {
                theme.popover_foreground
            };

            // The command in runs of matched/unmatched characters, so the
            // query's hits read highlighted inside the (possibly clipped) text.
            let mut spans: Vec<gpui::AnyElement> = Vec::new();
            let mut flush = |run: &mut String, hit: bool| {
                if run.is_empty() {
                    return;
                }
                spans.push(
                    div()
                        .flex_none()
                        .whitespace_nowrap()
                        .text_color(if hit { theme.blue } else { base })
                        .child(std::mem::take(run))
                        .into_any_element(),
                );
            };
            let mut pos = m.positions.iter().copied().peekable();
            let mut run = String::new();
            let mut run_hit = false;
            for (ci, ch) in line.chars().enumerate() {
                let hit = pos.next_if_eq(&ci).is_some();
                if hit != run_hit {
                    flush(&mut run, run_hit);
                    run_hit = hit;
                }
                run.push(ch);
            }
            flush(&mut run, run_hit);

            // Right column: a failure badge when the last run exited non-zero,
            // and how long ago that run was.
            let meta = self.history_meta.get(line);
            let failed = meta.and_then(|em| em.exit).filter(|&e| e != 0);
            let ago = meta
                .and_then(|em| em.ts)
                .map(|ts| super::history::format_ago(now, ts));

            div()
                .h(lh)
                .flex()
                .items_center()
                .gap_1p5()
                .px_2()
                .whitespace_nowrap()
                // Same selection fill as the completion menu (see the note
                // there on `list_active` vs the stock `accent`).
                .when(selected, |d| d.bg(theme.list_active))
                .child(div().flex_1().flex().overflow_hidden().children(spans))
                .when_some(failed, |d, code| {
                    d.child(
                        div()
                            .flex_none()
                            .text_color(theme.red)
                            .child(format!("✗ {code}")),
                    )
                })
                .when_some(ago, |d, ago| {
                    d.child(
                        div()
                            .flex_none()
                            .text_color(theme.muted_foreground)
                            .child(ago),
                    )
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
        let menu_h = lh * (line_count as f32) + px(10.);

        // Anchored at the line's left edge (unlike the completion menu, which
        // anchors at the current word): history rows are whole commands, so
        // the menu spans the input area at a fixed width — that keeps the
        // right-hand metadata column vertically aligned across rows. A small
        // gap keeps it clear of the input line and its caret.
        let gap = px(6.);
        let grid_w = self.cell_width * (total_cols as f32);
        let menu_w = if grid_w < px(720.) { grid_w } else { px(720.) };
        let y = if place_above {
            px(GRID_PAD_Y) + lh * (srow as f32) - menu_h - gap
        } else {
            px(GRID_PAD_Y) + lh * ((srow + 1) as f32) + gap
        };

        Some(
            div()
                .absolute()
                .left(px(GRID_PAD_X))
                .top(y)
                .flex()
                .flex_col()
                .py_1()
                .w(menu_w)
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

    /// The one-shot "shell integration didn't engage" notice (#46): a single
    /// floating line, bottom-right so it reads as a status aside rather than
    /// part of the prompt. Rendered whenever set — unlike the editor overlays
    /// it exists precisely because `input_active()` is false.
    fn render_integration_notice(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement + use<>> {
        let text = self.integration_notice.clone()?;
        let theme = cx.theme();
        Some(
            div()
                .absolute()
                .bottom(px(GRID_PAD_Y))
                .right(px(GRID_PAD_X))
                .max_w(px(560.))
                .px_3()
                .py_1()
                .bg(theme.popover)
                .border_1()
                .border_color(theme.border)
                .rounded(px(6.))
                .text_size(px(12.))
                .text_color(theme.muted_foreground)
                .child(text),
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

impl Drop for TerminalView {
    fn drop(&mut self) {
        // A history record still deferred when the pane goes away (tab closed,
        // window closed — possibly mid-command) is flushed rather than lost;
        // it carries an exit code only if the shell had already reported back.
        self.flush_pending_history();
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
        if self.shell_owns_prompt() {
            // A vi-mode (or handed-off) prompt never engages the editor:
            // release held gap input to the shell's own line editor (raw, no
            // typeahead record — there is no local adoption to reconcile
            // against) and drop any pending record without its `^U`. Those
            // bytes land on zle's line and are the shell's to keep; a record
            // surviving this prompt would flush at the next editor-engaged
            // prompt and resurrect long-consumed text into the editor.
            if let Some((_net, bytes)) = self.hold.release() {
                self.terminal.write(bytes);
            }
            self.typeahead.drain();
        } else if self.input_active() {
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
        let reverse_search_menu = self
            .input_active()
            .then(|| self.render_reverse_search_menu(cx))
            .flatten();
        // Not gated on `input_active()`: the notice explains why the editor
        // overlays are absent, so it renders exactly when they can't.
        let integration_notice = self.render_integration_notice(cx);

        // Captured for the right-click menu: the focus handle routes dispatched
        // actions to this terminal (and lets tab/split ones bubble to the root),
        // and the selection state greys out "Copy" / "Cut" when there's nothing
        // selected in either the grid or the prompt editor.
        let menu_focus = self.focus_handle.clone();
        let has_selection = self.any_selection();
        // Read at menu-open time (see the fork block below), so the fork rows'
        // enablement can't go stale between render and click — and so the
        // render path doesn't pay for an `agent_session()` clone every frame.
        let menu_view = cx.entity();

        div()
            .id("terminal-surface")
            .track_focus(&self.focus_handle)
            .key_context("Terminal")
            .size_full()
            .relative()
            .overflow_hidden()
            .px(px(GRID_PAD_X))
            .py(px(GRID_PAD_Y))
            // No background of its own: the window root paints the theme's
            // background (solid, gradient, or image — see `Tty7App::render`),
            // and default-background cells don't paint either, so it shows
            // through every pane. A surface-level fill here would both hide
            // gradients/images and double-composite a translucent theme's alpha.
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
            // Menu-dispatched copy leaves the selection up (`clear_on_copy:
            // false`) — only the dual-purpose ⌃C chord has to consume it.
            .on_action(cx.listener(|this, _: &CopyText, _w, cx| {
                this.copy_contextual(false, cx);
            }))
            .on_action(cx.listener(|this, _: &CutText, _w, cx| {
                this.cut_contextual(cx);
            }))
            .on_action(cx.listener(|this, _: &PasteText, _w, cx| this.paste_from_clipboard(cx)))
            .on_action(cx.listener(|this, _: &SelectAll, _w, cx| this.select_all_contextual(cx)))
            .on_action(cx.listener(|this, _: &UndoEdit, _w, cx| this.undo_edit(false, cx)))
            .on_action(cx.listener(|this, _: &RedoEdit, _w, cx| this.undo_edit(true, cx)))
            .on_action(
                cx.listener(|this, _: &FindInTerminal, window, cx| this.open_search(window, cx)),
            )
            // Find-again: step to the next / previous match. No-op when the bar is
            // closed (nothing to step through).
            .on_action(cx.listener(|this, _: &FindNext, _w, cx| {
                this.step_match(Direction::Right, cx);
            }))
            .on_action(cx.listener(|this, _: &FindPrevious, _w, cx| {
                this.step_match(Direction::Left, cx);
            }))
            .on_action(cx.listener(|this, _: &ClearScrollback, _w, cx| this.clear_scrollback(cx)))
            // Soft newline in the prompt editor (Shift+Enter / Alt+Enter by
            // default). Propagates when the editor isn't holding the line, so a
            // foreground application still sees the chord unchanged.
            .on_action(cx.listener(|this, _: &InsertNewline, _w, cx| {
                this.insert_newline_action(cx);
            }))
            // Tab / Shift-Tab are claimed here (in the "Terminal" key context) so
            // they reach the shell instead of triggering Root's focus navigation.
            // Tab → HT (0x09); Shift-Tab → CSI Z (back-tab), the standard sequence.
            // While the search field is focused it owns these keys, so propagate.
            .on_action(cx.listener(|this, _: &SendTab, _w, cx| {
                this.tab_pressed(true, cx);
            }))
            .on_action(cx.listener(|this, _: &SendBackTab, _w, cx| {
                this.tab_pressed(false, cx);
            }))
            .child(TerminalElement::new(entity))
            .children(search_bar)
            .children(input_bar)
            .children(completion_menu)
            .children(reverse_search_menu)
            .children(integration_notice)
            // Right-click context menu (gpui-component PopupMenu).
            .context_menu(move |menu, window, cx| {
                // Default (26px) rows: with the flat full-bleed highlight (no
                // floating pill, no inter-row gap) they read dense, not airy, and
                // match the command palette's row height. A fixed min-width keeps
                // the menu a consistent, intentional size instead of hugging the
                // longest label (which reads ragged).
                // Copy/Cut/Paste/Select All are dispatched inline (see
                // `handle_cmd_shortcut`) with no registered `KeyBinding`, so the menu
                // can't auto-derive their hints the way it does for the items below.
                // We render the hint ourselves via `menu_row_with_hint` to keep the
                // whole menu consistent, rather than register real bindings (which
                // would risk the Ctrl+C SIGINT fall-through on Windows/Linux).
                let menu = menu
                    .min_w(px(220.))
                    .action_context(menu_focus.clone())
                    .menu_element_with_disabled(
                        Box::new(CopyText),
                        !has_selection,
                        menu_row_with_hint("Copy", Some("secondary-c")),
                    )
                    // Cut is prompt-only; it shares Copy's enablement cue rather
                    // than offering a row that silently does nothing on output.
                    .menu_element_with_disabled(
                        Box::new(CutText),
                        !has_selection,
                        menu_row_with_hint("Cut", Some("secondary-x")),
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
                    // Find now has a real registered binding, so let the menu
                    // auto-render its shortcut hint (correct per platform) like the
                    // items below, instead of a hand-rolled mac-only one.
                    .menu("Find…", Box::new(FindInTerminal))
                    .menu("Clear", Box::new(ClearScrollback));

                // The fork block. Its rows dispatch actions that `Tty7App`
                // handles, so the submenu carries the same `action_context` as
                // the parent — a submenu is a menu of its own and does not
                // inherit it.
                //
                // Offered only for agents tty7 has a verified fork command for.
                // A *pane*-level ask is a spatial one, so this menu asks where
                // the fork goes; a tab-level ask is not, so the tab menu just
                // opens a new tab (issue #211).
                // Disabled — not hidden — until the session id is known, so the
                // capability stays discoverable when the agent's hooks aren't
                // installed; a remote pane can't fork at all, since the fork
                // command would run against the *local* agent.
                let view = menu_view.read(cx);
                let fork_label = view.agent().and_then(|a| a.fork_label());
                let can_fork = fork_label.is_some()
                    && view.remote_context().is_none()
                    && view.agent_session().is_some_and(|s| s.session_id.is_some());

                let menu = match fork_label {
                    Some(label) if can_fork => {
                        let focus = menu_focus.clone();
                        menu.separator()
                            .submenu(label, window, cx, move |submenu, _window, _cx| {
                                submenu
                                    .action_context(focus.clone())
                                    .menu("Split Right", Box::new(ForkAgentSessionRight))
                                    .menu("Split Left", Box::new(ForkAgentSessionLeft))
                                    .menu("Split Down", Box::new(ForkAgentSessionDown))
                                    .menu("Split Up", Box::new(ForkAgentSessionUp))
                            })
                    }
                    // Forkable agent, but nothing to fork *from* yet (no
                    // session id) or the wrong machine (a remote pane). A flat
                    // disabled row rather than an empty submenu: there is no
                    // placement to pick when the fork itself can't run.
                    Some(label) => menu
                        .separator()
                        .item(PopupMenuItem::new(label).disabled(true)),
                    None => menu,
                };

                menu.separator()
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
/// The char index where the whitespace-delimited word ending at `cursor` begins.
/// Mirrors the word-splitting the completion engine does, used when a completion
/// is all generators (no sync candidate to read the range off).
fn word_start_of(line: &str, cursor: usize) -> usize {
    let chars: Vec<char> = line.chars().collect();
    let mut start = cursor.min(chars.len());
    while start > 0 && !chars[start - 1].is_whitespace() {
        start -= 1;
    }
    start
}

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

/// What a finished mouse-selection gesture should auto-copy when
/// copy-on-select is enabled (see `Config::copy_on_select`).
#[derive(Debug, PartialEq)]
enum SelectEndCopy {
    /// Feature off, or the mouse-up ended no selection gesture (a plain
    /// click, a right/middle release): leave the clipboard alone.
    None,
    /// The gesture drove the terminal grid selection (drag / double / triple
    /// click over output): copy `term.selection`.
    Grid,
    /// The gesture landed on the command editor's line: copy the editor's
    /// own selection.
    Editor,
}

fn select_end_copy(enabled: bool, grid: bool, editor: bool) -> SelectEndCopy {
    match (enabled, grid, editor) {
        (false, ..) => SelectEndCopy::None,
        (true, true, _) => SelectEndCopy::Grid,
        (true, false, true) => SelectEndCopy::Editor,
        (true, false, false) => SelectEndCopy::None,
    }
}

fn open_file_path(path: &std::path::Path) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer"
    } else {
        "xdg-open"
    };
    if let Err(e) = std::process::Command::new(opener).arg(path).spawn() {
        log::warn!("failed to open {}: {e}", path.display());
    }
}

/// Run a user-configured file-open command for a clicked file link. The template
/// is expanded by [`expand_file_command_template`] and the first token is the
/// program; the rest are its arguments. Spawned detached — tty7 doesn't wait for
/// or read from the editor it launches.
fn run_file_command(
    template: &str,
    path: &std::path::Path,
    line: Option<u32>,
    column: Option<u32>,
) {
    let argv = expand_file_command_template(template, path, line, column);
    let Some((program, args)) = argv.split_first() else {
        log::warn!("link_file_command is empty; ignoring file link");
        return;
    };
    if let Err(e) = std::process::Command::new(program).args(args).spawn() {
        log::warn!("failed to run link_file_command {template:?}: {e}");
    }
}

/// Expand a file-open command template into an argv vector.
///
/// The template is split on whitespace into tokens. Within a token the
/// placeholders `{path}`, `{line}`, and `{column}` are replaced with their
/// values. If a token references a placeholder whose value is absent (e.g.
/// `{line}` for a link with no line number), the whole token is dropped — this
/// lets a combined token like `--line={line}` disappear cleanly rather than
/// leaving a dangling flag. `{path}` is always present, so a token that only
/// references `{path}` is never dropped.
fn expand_file_command_template(
    template: &str,
    path: &std::path::Path,
    line: Option<u32>,
    column: Option<u32>,
) -> Vec<String> {
    let path = path.to_string_lossy();
    template
        .split_whitespace()
        .filter_map(|token| expand_file_command_token(token, &path, line, column))
        .collect()
}

/// Substitute placeholders in a single template token, or return `None` if the
/// token references a placeholder with no value (so the caller drops it).
fn expand_file_command_token(
    token: &str,
    path: &str,
    line: Option<u32>,
    column: Option<u32>,
) -> Option<String> {
    let mut out = String::with_capacity(token.len());
    let mut rest = token;
    while let Some(open) = rest.find('{') {
        let Some(close_rel) = rest[open..].find('}') else {
            // No closing brace: the remainder is literal text.
            break;
        };
        let close = open + close_rel;
        out.push_str(&rest[..open]);
        let value = match &rest[open + 1..close] {
            "path" => Some(path.to_string()),
            "line" => line.map(|l| l.to_string()),
            "column" => column.map(|c| c.to_string()),
            // An unknown placeholder is left verbatim rather than dropping the
            // token, so a stray brace doesn't silently swallow an argument.
            other => Some(format!("{{{other}}}")),
        };
        // A recognized-but-absent placeholder drops the entire token.
        out.push_str(&value?);
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    Some(out)
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
/// Visual `(row, start-col, width)` of every char in the wrapped input line,
/// matching `render_input_bar`'s layout: char 0 starts at column `scol` (right
/// after the prompt), a `'\n'` is a hard break to column 0 of the next row (and
/// occupies no cell — width 0), and within a line a char that would overflow
/// wraps whole to column 0 of the next row. Also returns the pen `(row, col)`
/// after the last char, so callers can place the trailing end-of-line caret.
fn input_char_positions(
    chars: &[char],
    scol: usize,
    cols: usize,
) -> (Vec<(usize, usize, usize)>, usize, usize) {
    let mut positions: Vec<(usize, usize, usize)> = Vec::with_capacity(chars.len());
    let mut r = 0usize;
    let mut c = scol;
    for &ch in chars {
        if ch == '\n' {
            positions.push((r, c, 0));
            r += 1;
            c = 0;
            continue;
        }
        let w = display_width(ch).max(1);
        if c + w > cols {
            r += 1;
            c = 0;
        }
        positions.push((r, c, w));
        c += w;
    }
    (positions, r, c)
}

/// Visual size of the rendered input overlay: how many wrapped rows it
/// occupies and which of them carries the caret. Mirrors `render_input_bar`'s
/// layout: the IME pre-edit is inserted at the caret, and a one-cell caret
/// slot trails the buffer when the caret sits at the end (wrapping to a fresh
/// row when the content exactly fills its last one). The ghost autosuggestion
/// is deliberately excluded — the screen shouldn't scroll to reveal a
/// suggestion the user hasn't accepted.
fn input_overlay_rows(
    chars: &[char],
    cursor: usize,
    marked: &str,
    scol: usize,
    cols: usize,
) -> (usize, usize) {
    let mut merged: Vec<char> = Vec::with_capacity(chars.len() + marked.len());
    let cursor = cursor.min(chars.len());
    merged.extend_from_slice(&chars[..cursor]);
    merged.extend(marked.chars());
    merged.extend_from_slice(&chars[cursor..]);
    let (positions, r, c) = input_char_positions(&merged, scol, cols);
    let end_row = if cursor >= chars.len() && marked.is_empty() && c >= cols {
        r + 1
    } else {
        r
    };
    let caret_vrow = positions.get(cursor).map_or(end_row, |&(pr, _, _)| pr);
    (end_row + 1, caret_vrow)
}

/// Rows the grid (and the input overlay riding on it) must shift up so the
/// wrapped command editor stays visible when the prompt sits near the bottom:
/// enough that the overlay's last row lands on the last grid row — capped so
/// the caret's row never scrolls off the top when the input is taller than
/// the whole screen.
fn input_overflow_shift(crow: usize, caret_vrow: usize, visual_rows: usize, rows: usize) -> usize {
    (crow + visual_rows)
        .saturating_sub(rows)
        .min(crow + caret_vrow)
}

fn wrapped_click_index(
    chars: &[char],
    scol: usize,
    cols: usize,
    col: usize,
    target: usize,
    clamp: bool,
) -> Option<usize> {
    let len = chars.len();
    // `positions[i]` is the (row, start-col, width) of char `i`; `r`/`c` are the
    // pen position after the last char.
    let (positions, r, c) = input_char_positions(chars, scol, cols);
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
    // Past the row's content. If the row ends at a hard line break, snap to that
    // newline — the end of this logical line — rather than jumping onto the next
    // line. (A soft-wrapped row has no newline, so it continues below.)
    if let Some(last) = positions.iter().rposition(|&(pr, _, _)| pr == target) {
        if chars[last] == '\n' {
            return Some(last);
        }
    }
    // Otherwise the line soft-wraps: snap to the first char of the next visual
    // row, or the buffer end.
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
        LoopbackPlan, SelectEndCopy, WheelRoute, clipboard_paste_text, cwd_is_on_host,
        display_width, loopback_plan,
    };
    use super::{
        drag_scroll_step, encode_mouse, expand_file_command_template, fallback_chain,
        fig_icon_emoji, fig_icon_glyph, focus_report_bytes, input_overflow_shift,
        input_overlay_rows, menu_layout, paste_bytes, select_end_copy, shell_escape_path,
        smooth_scroll_step, submit_bytes, trim_trailing_spaces, wheel_route, wrapped_click_index,
    };
    use alacritty_terminal::term::TermMode;
    use gpui::{ClipboardEntry, ClipboardItem, ExternalPaths, Modifiers};
    use gpui_component::IconName;
    use std::path::{Path, PathBuf};

    // ── ⌘-click `localhost:PORT` routing ────────────────────────

    use crate::core::session::{RemoteTarget, WorkspaceId};
    use crate::daemon::protocol::RemoteKind;
    use crate::terminal::PaneWorkspace;

    fn ws(target: RemoteTarget, with_spec: bool) -> PaneWorkspace {
        PaneWorkspace {
            workspace: WorkspaceId::new(),
            target,
            spec: with_spec.then(|| {
                Box::new(
                    serde_json::from_str(
                        r#"{"host":"dev.box","port":22,"user":"me","auth_mode":"auto"}"#,
                    )
                    .unwrap(),
                )
            }),
        }
    }

    /// A plain local pane never forwards: `localhost:3000` there already means
    /// this machine.
    #[test]
    fn local_pane_opens_localhost_directly() {
        assert_eq!(loopback_plan(true, None, None, 1), LoopbackPlan::Direct);
    }

    /// An SSH pane forwards over its own connection, owned by the pane — the
    /// behaviour that shipped, unchanged.
    #[test]
    fn ssh_pane_forwards_on_the_pane() {
        assert_eq!(
            loopback_plan(true, None, Some(RemoteKind::NativeSsh), 7),
            LoopbackPlan::ForwardOnPane(7)
        );
        // A non-native remote pane (a plain `ssh` typed into a shell) has no
        // russh connection to forward over.
        assert_eq!(
            loopback_plan(true, None, Some(RemoteKind::Wsl), 7),
            LoopbackPlan::Direct
        );
    }

    /// A remote-workspace pane forwards over the *workspace's* connection. It
    /// carries no `RemoteContext` at all — its shell is local to the remote
    /// daemon — which is exactly why the workspace has to be consulted first.
    #[test]
    fn remote_workspace_pane_forwards_on_the_workspace() {
        let w = ws(RemoteTarget::direct("me", "dev.box", 22), true);
        assert_eq!(
            loopback_plan(true, Some(&w), None, 7),
            LoopbackPlan::ForwardOnWorkspace(Box::new(w.clone())),
            "no RemoteContext, but still forwarded"
        );
        // And the workspace wins over a pane-level answer.
        assert_eq!(
            loopback_plan(true, Some(&w), Some(RemoteKind::NativeSsh), 7),
            LoopbackPlan::ForwardOnWorkspace(Box::new(w))
        );
    }

    /// **The WSL exception.** WSL shares `localhost` with its
    /// Windows host, so the URL already resolves — building a forward would be
    /// pure overhead. Wired now; M8 supplies the target.
    #[test]
    fn wsl_workspace_needs_no_forward() {
        let w = ws(
            RemoteTarget::Wsl {
                distro: "Ubuntu".into(),
            },
            false,
        );
        assert_eq!(
            loopback_plan(true, Some(&w), None, 7),
            LoopbackPlan::NoForwardNeeded
        );
    }

    /// A non-WSL workspace with no connection spec cannot be forwarded over.
    /// Opening the *client's* `localhost` instead would be a wrong answer that
    /// looks right, so the link is left alone.
    #[test]
    fn workspace_without_a_spec_does_not_forward() {
        let w = ws(RemoteTarget::direct("me", "dev.box", 22), false);
        assert_eq!(loopback_plan(true, Some(&w), None, 7), LoopbackPlan::Direct);
    }

    /// The `ssh_loopback_forward` off switch disables every route, including the
    /// hover underline that `can_forward_loopback` drives off the same plan.
    #[test]
    fn the_off_switch_disables_every_route() {
        let w = ws(RemoteTarget::direct("me", "dev.box", 22), true);
        assert_eq!(
            loopback_plan(false, Some(&w), None, 7),
            LoopbackPlan::Direct
        );
        assert_eq!(
            loopback_plan(false, None, Some(RemoteKind::NativeSsh), 7),
            LoopbackPlan::Direct
        );
    }

    #[test]
    fn file_command_template_substitutes_path_line_and_column() {
        let argv = expand_file_command_template(
            "herdr edit {path} --line={line} --column={column}",
            Path::new("/tmp/foo.rs"),
            Some(42),
            Some(7),
        );
        assert_eq!(
            argv,
            vec!["herdr", "edit", "/tmp/foo.rs", "--line=42", "--column=7",]
        );
    }

    #[test]
    fn file_command_template_drops_tokens_for_absent_values() {
        // No line/column: the combined flag tokens vanish entirely, leaving no
        // dangling `--line` for the downstream parser.
        let argv = expand_file_command_template(
            "herdr edit {path} --line={line} --column={column}",
            Path::new("/tmp/foo.rs"),
            None,
            None,
        );
        assert_eq!(argv, vec!["herdr", "edit", "/tmp/foo.rs"]);

        // Column absent but line present: only the column flag drops.
        let argv = expand_file_command_template(
            "herdr edit {path} --line={line} --column={column}",
            Path::new("/tmp/foo.rs"),
            Some(42),
            None,
        );
        assert_eq!(argv, vec!["herdr", "edit", "/tmp/foo.rs", "--line=42"]);
    }

    #[test]
    fn file_command_template_keeps_path_only_token_and_unknown_placeholder() {
        // A path-only program still runs; an unknown placeholder is left verbatim
        // rather than dropping its token.
        let argv = expand_file_command_template(
            "code --goto {path}:{line} {other}",
            Path::new("/tmp/foo.rs"),
            None,
            None,
        );
        // `{path}:{line}` drops (line absent); `{other}` stays literal.
        assert_eq!(argv, vec!["code", "--goto", "{other}"]);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn clipboard_image_transcodes_bmp_to_png_and_passes_png_through() {
        use gpui::{Image, ImageFormat};

        // A BMP (what a Windows screenshot lands as) must be re-encoded to PNG,
        // since agent vision rejects BMP. Build one with the image crate.
        let pixel = image::RgbaImage::from_pixel(1, 1, image::Rgba([1, 2, 3, 255]));
        let mut bmp = Vec::new();
        image::DynamicImage::ImageRgba8(pixel)
            .write_to(&mut std::io::Cursor::new(&mut bmp), image::ImageFormat::Bmp)
            .unwrap();
        let path = super::write_clipboard_image(&Image::from_bytes(ImageFormat::Bmp, bmp)).unwrap();
        assert_eq!(path.extension().unwrap(), "png");
        // PNG magic number: the staged file is genuinely a PNG, not renamed BMP.
        assert_eq!(&std::fs::read(&path).unwrap()[..8], b"\x89PNG\r\n\x1a\n");

        // A format agents already accept is written through byte-for-byte.
        let png = std::fs::read(&path).unwrap();
        let out = super::write_clipboard_image(&Image::from_bytes(ImageFormat::Png, png.clone()))
            .unwrap();
        assert_eq!(out.extension().unwrap(), "png");
        assert_eq!(std::fs::read(&out).unwrap(), png);
    }

    /// The bundled Hack always anchors the fallback chain so prompt symbols
    /// (`➜`, `❯`, powerline wedges) never fall through to the OS cascade —
    /// unless the user already covers it as primary or in their own list.
    #[test]
    fn fallback_chain_pins_bundled_hack_last() {
        let configured = vec!["Menlo".to_string(), "Apple Color Emoji".to_string()];

        // A custom primary that may lack the prompt symbols → Hack appended.
        let chain = fallback_chain("JetBrains Mono", &configured);
        assert_eq!(chain[..2], ["Menlo", "Apple Color Emoji"]);
        assert_eq!(chain.last().unwrap(), "Hack");

        // Hack as the primary face already covers everything it could add.
        let chain = fallback_chain("Hack", &configured);
        assert_eq!(chain[..2], ["Menlo", "Apple Color Emoji"]);
        assert!(!chain.iter().any(|f| f == "Hack"));

        // A user who lists Hack explicitly keeps their chosen position.
        let with_hack = vec!["Hack".to_string(), "Menlo".to_string()];
        let chain = fallback_chain("SF Mono", &with_hack);
        assert_eq!(chain[..2], ["Hack", "Menlo"]);

        // "Hack Nerd Font" is a different family — the bundled face still lands.
        assert_eq!(
            fallback_chain("Hack Nerd Font", &[]).last().unwrap(),
            "Hack",
            "a Hack-prefixed family name must not suppress the bundled anchor"
        );
    }

    /// Hack has no ideographs, so a chain naming only faces this OS lacks sends
    /// every CJK glyph into the platform cascade — where a 1.0em face gets
    /// left-aligned inside `element.rs`'s 1.2041em two-column slot. The stock
    /// names have to be in the chain even for a config written before the fix.
    #[test]
    fn fallback_chain_appends_platform_stock_faces() {
        let stock = crate::core::config::platform_last_resort_fallbacks();
        assert!(!stock.is_empty(), "every platform needs a CJK last resort");

        // The pre-fix default: macOS-only names, nothing Windows/Linux can match.
        let legacy = vec![
            "Menlo".to_string(),
            "Hasklug Nerd Font Mono".to_string(),
            "Maple Mono NF CN".to_string(),
            "Apple Color Emoji".to_string(),
        ];
        let chain = fallback_chain("Hack", &legacy);
        for name in stock {
            assert!(
                chain.iter().any(|f| f == name),
                "{name} missing from repaired chain {chain:?}"
            );
        }

        // The user's own order is never displaced — stock faces land behind it.
        assert_eq!(chain[..legacy.len()], legacy[..]);

        // Already-listed stock faces aren't duplicated.
        let explicit = vec![stock[0].to_string()];
        let chain = fallback_chain("Hack", &explicit);
        assert_eq!(
            chain.iter().filter(|f| *f == stock[0]).count(),
            1,
            "stock face duplicated in {chain:?}"
        );

        // A stock face chosen as the *primary* isn't re-added as its own fallback.
        assert!(!fallback_chain(stock[0], &[]).iter().any(|f| f == stock[0]));
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

    /// Copy-on-select fires only when the released gesture actually drove a
    /// selection, and copies the buffer that gesture touched — the terminal
    /// grid or the command editor's line. Off, or a mouse-up that ended no
    /// gesture (a plain click, a right-click), must leave the clipboard alone.
    #[test]
    fn copy_on_select_copies_the_buffer_the_gesture_touched() {
        // Disabled: never copy, whatever kind of gesture just ended.
        assert_eq!(select_end_copy(false, true, false), SelectEndCopy::None);
        assert_eq!(select_end_copy(false, false, true), SelectEndCopy::None);

        // A grid gesture (drag / double / triple click over output) copies
        // the terminal selection; one on the editor line copies the editor's.
        assert_eq!(select_end_copy(true, true, false), SelectEndCopy::Grid);
        assert_eq!(select_end_copy(true, false, true), SelectEndCopy::Editor);

        // No gesture ended → nothing to copy.
        assert_eq!(select_end_copy(true, false, false), SelectEndCopy::None);

        // The press routes to exactly one buffer, but if both flags ever
        // read set, the grid selection (the visible one) wins.
        assert_eq!(select_end_copy(true, true, true), SelectEndCopy::Grid);
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
    fn submit_bytes_sends_a_multi_line_command_as_one_bracketed_paste() {
        // The regression this exists for: replaying each newline as its own CR
        // made zle run a full prompt cycle per line (preexec + the user's
        // precmd chain + a highlight pass), so a pasted block crawled down the
        // screen. One paste, one CR — one cycle, whatever the line count.
        assert_eq!(
            submit_bytes("echo a\necho b\necho c", true),
            b"\x1b[200~echo a\necho b\necho c\x1b[201~\r".to_vec()
        );
        // Exactly one CR reaches the shell: the accept, not one per line.
        let out = submit_bytes("a\nb\nc\nd", true);
        assert_eq!(out.iter().filter(|&&b| b == b'\r').count(), 1);
        // Single-line commands take the same shape — no special case.
        assert_eq!(
            submit_bytes("ls -la", true),
            b"\x1b[200~ls -la\x1b[201~\r".to_vec()
        );
    }

    #[test]
    fn submit_bytes_falls_back_to_per_line_cr_without_bracketed_paste() {
        // A shell that never enabled bracketed paste can only assemble a
        // multi-line command the old way: one Enter per line, letting its
        // editor do the PS2 continuation.
        assert_eq!(submit_bytes("a\nb", false), b"a\rb\r".to_vec());
        // A CRLF clipboard yields one CR per line, not a stray extra Enter
        // (`\r\n` used to become `\r\r` — a blank line submitted mid-command).
        assert_eq!(submit_bytes("a\r\nb", false), b"a\rb\r".to_vec());
    }

    #[test]
    fn submit_bytes_normalizes_line_breaks_inside_the_paste() {
        // The CR of a CRLF clipboard must not ride inside the markers either:
        // zsh turns a pasted CR into a newline (so the block would gain a blank
        // line) and a shell that doesn't leaves a literal `^M` in the command.
        assert_eq!(
            submit_bytes("a\r\nb", true),
            b"\x1b[200~a\nb\x1b[201~\r".to_vec()
        );
        // A lone CR is a line break too — dropping it would glue the lines
        // together into one command.
        assert_eq!(
            submit_bytes("a\rb", true),
            b"\x1b[200~a\nb\x1b[201~\r".to_vec()
        );
        assert_eq!(submit_bytes("a\rb", false), b"a\rb\r".to_vec());
    }

    #[test]
    fn submit_bytes_strips_esc_and_skips_markers_on_an_empty_line() {
        // Clipboard text carrying its own `ESC[201~` would otherwise close the
        // paste early and have the rest run as typed input.
        let out = submit_bytes("foo\x1b[201~\nrm -rf ~", true);
        let end = b"\x1b[201~";
        assert_eq!(out.windows(end.len()).filter(|w| *w == end).count(), 1);
        assert_eq!(out, b"\x1b[200~foo[201~\nrm -rf ~\x1b[201~\r".to_vec());
        // ESC is stripped on the unbracketed path too — raw ESC reaching zle is
        // an editor command, not text.
        assert_eq!(submit_bytes("a\x1bb", false), b"ab\r".to_vec());

        // An empty line is a bare Enter: zsh's `bracketed-paste-magic` errors
        // on a paste with nothing between the markers.
        assert_eq!(submit_bytes("", true), b"\r".to_vec());
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
    fn wrapped_click_index_treats_newlines_as_hard_breaks() {
        // "a\nbc" after a 4-col prompt lays out as row 0 = "a" (col 4) and
        // row 1 = "bc" (cols 0..2). Indices: 0='a', 1='\n', 2='b', 3='c'.
        assert_eq!(click("a\nbc", 4, 80, 4, 0), Some(0)); // 'a'
        assert_eq!(click("a\nbc", 4, 80, 0, 1), Some(2)); // 'b' on the next line
        assert_eq!(click("a\nbc", 4, 80, 1, 1), Some(3)); // 'c'
        // Clicking past the end of the first line snaps to the newline (the end
        // of that logical line), not onto the second line.
        assert_eq!(click("a\nbc", 4, 80, 40, 0), Some(1));
        // Past the last line's content → buffer end.
        assert_eq!(click("a\nbc", 4, 80, 40, 1), Some(4));
        // A blank line in the middle ("a\n\nb") is its own row; clicking it lands
        // on that empty line rather than falling through to "b".
        // Indices: 0='a', 1='\n', 2='\n', 3='b'. Row 1 holds the second newline.
        assert_eq!(click("a\n\nb", 4, 80, 3, 1), Some(2));
        assert_eq!(click("a\n\nb", 4, 80, 0, 2), Some(3)); // 'b' on row 2
    }

    #[test]
    fn input_overlay_rows_counts_wraps_slot_marked_and_newlines() {
        let rows = |text: &str, cursor: usize, marked: &str, scol: usize, cols: usize| {
            let chars: Vec<char> = text.chars().collect();
            input_overlay_rows(&chars, cursor, marked, scol, cols)
        };
        // Empty input: just the caret slot on the prompt row.
        assert_eq!(rows("", 0, "", 3, 8), (1, 0));
        // 10 chars after a 6-col prompt in an 8-col grid fill rows 0..=1
        // exactly, so the end-of-line caret slot wraps to row 2.
        assert_eq!(rows("aaaaaaaaaa", 10, "", 6, 8), (3, 2));
        // Same content with the caret in the middle: no trailing slot beyond
        // the content, and the caret sits on the char's own row.
        assert_eq!(rows("aaaaaaaaaa", 3, "", 6, 8), (2, 1));
        // A hard newline is its own break; caret at the end lands on row 1.
        assert_eq!(rows("ab\ncd", 5, "", 0, 8), (2, 1));
        // IME pre-edit is inserted at the caret and counts its display width:
        // the two-cell 漢 doesn't fit in the last column of row 0, so it wraps
        // whole — pulling the caret's row down with it.
        assert_eq!(rows("ab", 1, "漢", 6, 8), (2, 1));
    }

    #[test]
    fn input_overflow_shift_keeps_the_tail_and_caret_visible() {
        // Fits: a 3-row input anchored at row 5 of a 22-row grid.
        assert_eq!(input_overflow_shift(5, 2, 3, 22), 0);
        // Spills one row past the bottom → shift up by one.
        assert_eq!(input_overflow_shift(20, 2, 3, 22), 1);
        // Taller than the whole screen, caret at the end: shift so the last
        // row lands on the last grid row (caret stays visible with it).
        assert_eq!(input_overflow_shift(21, 29, 30, 22), 29);
        // Same giant input with the caret back on its first row: the cap
        // stops the caret row from scrolling off the top.
        assert_eq!(input_overflow_shift(21, 0, 30, 22), 21);
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

    /// The gate that keeps a remote path away from a `git` that cannot see it.
    ///
    /// Only the two *agreeing* pairings answer yes. The third row is the one
    /// that matters — a shell on another machine paired with the local host —
    /// because handing `/home/me/proj` to the local `git` does not fail
    /// cleanly: on Windows that path is drive-relative and quietly resolves to
    /// `C:\\home\\me\\proj`, so an unrelated local repository's branch and diff
    /// would be reported as the remote pane's own.
    #[test]
    fn only_a_matching_host_may_answer_for_a_panes_paths() {
        // Local shell on the local host: the ordinary case.
        assert!(cwd_is_on_host(false, true));
        // Remote shell on a host that is that machine: a remote-workspace pane,
        // whose git line, diff and worktree offer all hang off this row.
        assert!(cwd_is_on_host(true, false));

        // Remote shell still paired with the local host — a native-SSH or WSL
        // pane, which has no `Host` behind it: refused.
        assert!(!cwd_is_on_host(true, true));
        // Local shell paired with a remote host: equally meaningless.
        assert!(!cwd_is_on_host(false, false));
    }

    /// A pane's machine comes from its workspace, and the two move together:
    /// [`TerminalView::set_workspace`] is the only thing that sets either, so a
    /// pane cannot end up running its shell on one machine and asking `git` on
    /// another.
    ///
    /// Checked against `PaneWorkspace` directly rather than through a live view
    /// (which needs a window, a daemon and a pane): the derivation under test is
    /// the target → `HostId` one, and pinning it here is what catches a future
    /// `set_workspace` that forgets the host half. The ids must agree with what
    /// `RemoteConnections::insert` registered — same `connection_key`, checked
    /// by `connection_keys_match_the_contract_table` in `tty7-core`.
    #[test]
    fn a_panes_host_is_its_workspaces_machine() {
        use crate::core::session::{RemoteTarget, WorkspaceId};
        use crate::ui::host_ops::HostId;

        let target = RemoteTarget::Alias {
            alias: "build-box".into(),
        };
        let ws = PaneWorkspace {
            workspace: WorkspaceId::new(),
            target: target.clone(),
            spec: None,
        };

        // What `set_workspace` computes, for each of its two inputs.
        let remote = ws.target.host_id();
        assert_eq!(remote, target.host_id(), "the workspace's own machine");
        assert!(!remote.is_local(), "a remote workspace is not this machine");
        assert_eq!(
            HostId::from_connection_key("ssh-alias:build-box"),
            remote,
            "the id the connection was opened under, or the registry lookup misses"
        );

        // Two workspaces on one box are one machine — one connection, one host
        // object, one git probe shared by both.
        let sibling = PaneWorkspace {
            workspace: WorkspaceId::new(),
            target,
            spec: None,
        };
        assert_eq!(sibling.target.host_id(), remote);
    }
}

/// A pane the UI-level gpui tests can put in a tab: a real [`TerminalView`] on a
/// socketpair with nothing on the far end, so a test window has a pane without a
/// daemon, a shell, or a byte of output to repaint for. That silence is the
/// point — the render-idle tests measure what the *window* does when nothing is
/// happening, so the pane must not be a source of frames.
///
/// The caller keeps the returned stream alive: dropping it closes the socket and
/// the reader retires the pane.
#[cfg(all(test, unix))]
pub(crate) fn quiet_test_pane(
    pane_id: u64,
    window: &mut Window,
    cx: &mut gpui::App,
) -> (gpui::Entity<TerminalView>, std::os::unix::net::UnixStream) {
    let (client_side, daemon_side) = std::os::unix::net::UnixStream::pair().unwrap();
    let terminal = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24))
        .expect("socketpair-backed terminal");
    let view = cx.new(|cx| TerminalView::with_terminal(terminal, pane_id, window, cx));
    (view, daemon_side)
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

    /// Report the shell as idle at its prompt and wait for the view to see it,
    /// so `input_active()` is true and the local command editor owns the line.
    fn prompt_ready(
        window: &gpui::WindowHandle<TerminalView>,
        cx: &mut TestAppContext,
        daemon: &mut UnixStream,
    ) {
        DaemonMsg::Prompt {
            active: true,
            at_prompt: true,
            last_exit: None,
        }
        .encode(daemon)
        .unwrap();
        for _ in 0..200 {
            if window
                .update(cx, |view, _, _| view.terminal.at_prompt())
                .unwrap()
            {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("the prompt report never reached the view");
    }

    /// **A session id the agent reports raises [`AgentSessionChanged`], and it
    /// does so without the status moving.** That is the whole point: the id
    /// arrives on the agent's hooks, minutes after anything structural happened
    /// to the window, and nothing else was going to make the layout save. A
    /// record with no session id in it is a workspace that cannot resume, which
    /// is what made resume-after-End-Sessions look intermittent.
    ///
    /// The second poll must stay quiet — a save (and, on a remote workspace, a
    /// push to the machine) per repaint would be a different bug.
    #[gpui::test]
    fn a_reported_session_id_asks_the_window_to_save(cx: &mut TestAppContext) {
        use crate::core::cli_agent::{AgentSessionState, AgentStatus};

        crate::core::config::pin_test_config_dir();
        let (window, mut daemon) = harness(cx);
        let saves = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let view = window.update(cx, |_, _, cx| cx.entity()).unwrap();
        {
            let saves = saves.clone();
            cx.update(|cx| {
                cx.subscribe(&view, move |_, _: &AgentSessionChanged, _| {
                    saves.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                })
                .detach();
            });
        }

        // The hooks report a conversation. `status` is `Idle` before and after,
        // so a poll keyed only on the status would never notice.
        DaemonMsg::AgentStatus(Some(AgentSessionState {
            status: AgentStatus::Idle,
            message: None,
            session_id: Some("sid-abc".into()),
            launch_argv: Some(vec!["claude".into()]),
            rich: true,
            cwd: None,
            activity: 0,
        }))
        .encode(&mut daemon)
        .unwrap();
        for _ in 0..200 {
            if window
                .update(cx, |view, _, _| view.terminal.agent_session().is_some())
                .unwrap()
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        window
            .update(cx, |view, _, cx| view.poll_agent_status(false, cx))
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            saves.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the id has to reach the layout on file"
        );

        window
            .update(cx, |view, _, cx| view.poll_agent_status(false, cx))
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            saves.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "an unchanged session must not re-save on every poll"
        );
    }

    /// A hover cell remembered while the pane was tall names a row the grid no
    /// longer has once the pane shrinks (a vertical split, a smaller window).
    /// Resolving it must decline rather than index the grid — this path runs
    /// from `ModifiersChanged` (the ⌘ of the very ⌘⇧D that split the pane), an
    /// `extern "C"` callback where the panic can't unwind and aborts the app.
    #[gpui::test]
    fn a_stale_hover_row_does_not_index_the_shrunken_grid(cx: &mut TestAppContext) {
        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, _, cx| {
                // Hover the last row of the 24-row grid, then shrink to 8 rows.
                view.hover_link_at(0, 23, true, cx);
                view.terminal.resize(TermSize::new(80, 8), 8, 17);
                // `set_grid_size` drops the stale cell in the real app; pin it
                // here so the guard inside the lookup is what's under test.
                view.last_hover_cell = Some((0, 23));
                assert!(
                    !view.refresh_link_hover(true, cx),
                    "a row outside the grid can't hold a link"
                );
            })
            .unwrap();
    }

    /// The other half of the fix: the pane that shrank forgets the hover it was
    /// holding, rather than carrying a cell (and the underline it resolved) that
    /// now names different text.
    #[gpui::test]
    fn a_resize_forgets_the_hovered_cell(cx: &mut TestAppContext) {
        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, _, cx| {
                // Pin a known geometry first — what the test window measured for
                // itself is the element's business, and this is about the
                // transition. Then hover the last row of those 24.
                view.set_grid_size(80, 24, px(8.), px(17.));
                view.hover_link_at(0, 23, true, cx);
                assert_eq!(view.last_hover_cell, Some((0, 23)));
                view.hovered_link = Some(HoveredLink {
                    start: Point::new(Line(23), Column(0)),
                    end: Point::new(Line(23), Column(3)),
                });
                // The same geometry again changes nothing...
                view.set_grid_size(80, 24, px(8.), px(17.));
                assert_eq!(view.last_hover_cell, Some((0, 23)));
                // ...but a split (or a window drag) that shrinks the pane does.
                view.set_grid_size(80, 8, px(8.), px(17.));
                assert!(view.last_hover_cell.is_none(), "the cell is stale");
                assert!(view.hovered_link.is_none(), "so is the link it resolved");
            })
            .unwrap();
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

    /// Deliver one printable character the way the running platform actually
    /// does. macOS hands all text to the input context, which arrives as
    /// `commit_text` (see `input::defer_to_ime`); elsewhere it travels the
    /// `on_key_down` / `key_char` path. Tests that assert on *text* input must
    /// go through here, or they exercise a path the platform never takes.
    fn type_char(
        view: &mut TerminalView,
        ch: &str,
        window: &mut Window,
        cx: &mut Context<TerminalView>,
    ) {
        if cfg!(target_os = "macos") {
            let _ = window;
            view.commit_text(ch, cx);
        } else {
            let ev = KeyDownEvent {
                keystroke: gpui::Keystroke {
                    modifiers: gpui::Modifiers::default(),
                    key: ch.to_string(),
                    key_char: Some(ch.to_string()),
                },
                is_held: false,
                prefer_character_input: false,
            };
            view.on_key_down(&ev, window, cx);
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

    #[gpui::test]
    fn shell_vi_mode_prompt_bypasses_the_local_editor(cx: &mut TestAppContext) {
        let (window, mut daemon) = harness(cx);
        DaemonMsg::Prompt {
            active: true,
            at_prompt: true,
            last_exit: None,
        }
        .encode(&mut daemon)
        .unwrap();
        DaemonMsg::Output(b"\x1b]133;V;1\x07\x1b]133;B\x07".to_vec())
            .encode(&mut daemon)
            .unwrap();

        for _ in 0..200 {
            cx.run_until_parked();
            let ready = window
                .update(cx, |view, _, _| {
                    view.terminal.shell_vi_mode() && view.terminal.zle_reading()
                })
                .unwrap();
            if ready {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        window
            .update(cx, |view, window, cx| {
                assert!(
                    !view.input_active(),
                    "shell vi-mode lets the shell line editor own prompt input"
                );
                type_char(view, "a", window, cx);
                assert_eq!(
                    view.cmd.text(),
                    "",
                    "vi-mode prompt input must not draw through the local overlay"
                );
            })
            .unwrap();
        assert_eq!(
            next_input_until_timeout(&mut daemon),
            Some(b"a".to_vec()),
            "shell vi-mode prompt input must reach the shell directly"
        );

        DaemonMsg::Prompt {
            active: true,
            at_prompt: true,
            last_exit: Some(0),
        }
        .encode(&mut daemon)
        .unwrap();
        DaemonMsg::Output(b"\x1b]133;V;0\x07\x1b]133;B\x07".to_vec())
            .encode(&mut daemon)
            .unwrap();
        for _ in 0..200 {
            cx.run_until_parked();
            let active = window.update(cx, |view, _, _| view.input_active()).unwrap();
            if active {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("an emacs-mode prompt should re-enable tty7's local editor");
    }

    /// Wait until the daemon-fed prompt state makes the local editor live.
    fn wait_for_input_active(window: &gpui::WindowHandle<TerminalView>, cx: &mut TestAppContext) {
        for _ in 0..200 {
            cx.run_until_parked();
            let active = window.update(cx, |view, _, _| view.input_active()).unwrap();
            if active {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("the local editor never engaged at the prompt");
    }

    /// Tab the engine has nothing for must not be swallowed (#136): the
    /// locally edited line is shipped to the shell followed by the Tab
    /// itself, and the local editor stays out of the way until the shell
    /// reports its next prompt — from there the shell's own completion owns
    /// the line.
    #[gpui::test]
    fn tab_with_no_candidates_hands_the_line_to_the_shell(cx: &mut TestAppContext) {
        let (window, mut daemon) = harness(cx);
        DaemonMsg::Prompt {
            active: true,
            at_prompt: true,
            last_exit: None,
        }
        .encode(&mut daemon)
        .unwrap();
        wait_for_input_active(&window, cx);

        window
            .update(cx, |view, window, cx| {
                // A command-position word matching no builtin or $PATH entry,
                // so the completion engine returns `None`.
                for ch in ["z", "z", "q", "q", "x"] {
                    type_char(view, ch, window, cx);
                }
                assert_eq!(view.cmd.text(), "zzqqx");
                view.complete_tab(true, cx);
                assert_eq!(view.cmd.text(), "", "the line moved to the shell");
                assert!(
                    !view.input_active(),
                    "the shell owns the prompt after the handoff"
                );
            })
            .unwrap();
        assert_eq!(
            next_input_until_timeout(&mut daemon),
            Some(b"zzqqx".to_vec()),
            "the edited line ships ahead of the Tab"
        );
        assert_eq!(
            next_input_until_timeout(&mut daemon),
            Some(b"\t".to_vec()),
            "the Tab reaches the PTY instead of being swallowed"
        );

        // A same-prompt redraw (a prompt framework re-emitting the
        // PS1-embedded `133;B` on reset-prompt / a completion list reprint)
        // must NOT re-engage the editor — zle still holds the handed-off
        // text, and an engaged-empty editor would fork the two buffers.
        DaemonMsg::Prompt {
            active: true,
            at_prompt: true,
            last_exit: None,
        }
        .encode(&mut daemon)
        .unwrap();
        for _ in 0..200 {
            cx.run_until_parked();
            let applied = window
                .update(cx, |view, _, _| view.terminal.prompt_seq() >= 2)
                .unwrap();
            if applied {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        window
            .update(cx, |view, _, _| {
                assert!(
                    !view.input_active(),
                    "a same-prompt redraw must not re-engage the editor"
                );
            })
            .unwrap();

        // A real command cycle — the shell leaves the prompt and comes back —
        // re-engages the local editor.
        DaemonMsg::Prompt {
            active: true,
            at_prompt: false,
            last_exit: None,
        }
        .encode(&mut daemon)
        .unwrap();
        DaemonMsg::Prompt {
            active: true,
            at_prompt: true,
            last_exit: Some(0),
        }
        .encode(&mut daemon)
        .unwrap();
        wait_for_input_active(&window, cx);
    }

    /// With `tab_completion` off, Tab never opens tty7's menu — even when the
    /// engine would have candidates, the line and the Tab go to the shell.
    #[gpui::test]
    fn tab_completion_off_sends_every_tab_to_the_shell(cx: &mut TestAppContext) {
        let (window, mut daemon) = harness(cx);
        cx.update(|cx| {
            let mut cfg = cx.global::<Config>().clone();
            cfg.tab_completion = false;
            cx.set_global(cfg);
        });
        DaemonMsg::Prompt {
            active: true,
            at_prompt: true,
            last_exit: None,
        }
        .encode(&mut daemon)
        .unwrap();
        wait_for_input_active(&window, cx);

        window
            .update(cx, |view, window, cx| {
                // "cd " would offer path candidates were the engine consulted.
                for ch in ["c", "d", " "] {
                    type_char(view, ch, window, cx);
                }
                view.complete_tab(true, cx);
                assert!(view.completion.is_none(), "no tty7 menu while opted out");
                assert_eq!(view.cmd.text(), "");
            })
            .unwrap();
        assert_eq!(next_input_until_timeout(&mut daemon), Some(b"cd ".to_vec()));
        assert_eq!(next_input_until_timeout(&mut daemon), Some(b"\t".to_vec()));
    }

    #[gpui::test]
    fn shell_vi_mode_prompt_input_is_not_typeahead(cx: &mut TestAppContext) {
        let (window, mut daemon) = harness(cx);
        DaemonMsg::Prompt {
            active: true,
            at_prompt: true,
            last_exit: None,
        }
        .encode(&mut daemon)
        .unwrap();
        DaemonMsg::Output(b"\x1b]133;V;1\x07\x1b]133;B\x07".to_vec())
            .encode(&mut daemon)
            .unwrap();

        for _ in 0..200 {
            cx.run_until_parked();
            let ready = window
                .update(cx, |view, _, _| {
                    !view.input_active()
                        && view.terminal.shell_vi_mode()
                        && view.terminal.zle_reading()
                })
                .unwrap();
            if ready {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        window
            .update(cx, |view, window, cx| {
                type_char(view, "i", window, cx);
            })
            .unwrap();
        assert_eq!(
            next_input_until_timeout(&mut daemon),
            Some(b"i".to_vec()),
            "vi prompt input is normal shell input, not deferred gap typeahead"
        );

        DaemonMsg::Output(b"\x1b]133;V;0\x07\x1b]133;B\x07".to_vec())
            .encode(&mut daemon)
            .unwrap();
        for _ in 0..200 {
            cx.run_until_parked();
            let active = window.update(cx, |view, _, _| view.input_active()).unwrap();
            if active {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        window
            .update(cx, |view, _, _| assert!(view.input_active()))
            .unwrap();
        assert_eq!(
            next_input_until_timeout(&mut daemon),
            None,
            "leaving shell vi-mode must not flush a stale typeahead wipe"
        );
    }

    /// Text typed during a command gap is held for the next prompt's editor —
    /// but a vi prompt never engages the editor, so the hold must be released
    /// raw (the shell's own line editor consumes it) and the typeahead record
    /// dropped. Without that, the record lingers past the whole vi prompt and
    /// flushes at the next emacs-mode prompt: a spurious `^U` plus the long-
    /// consumed gap text resurrected into the local editor.
    #[gpui::test]
    fn shell_vi_mode_prompt_releases_gap_hold_without_stale_typeahead(cx: &mut TestAppContext) {
        let (window, mut daemon) = harness(cx);
        // Shell integration live, a command running: gap input gets held.
        DaemonMsg::Prompt {
            active: true,
            at_prompt: false,
            last_exit: None,
        }
        .encode(&mut daemon)
        .unwrap();
        for _ in 0..200 {
            cx.run_until_parked();
            let gap = window
                .update(cx, |view, _, _| {
                    view.terminal.shell_active() && !view.terminal.at_prompt()
                })
                .unwrap();
            if gap {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        window
            .update(cx, |view, _, cx| view.commit_text("ls", cx))
            .unwrap();

        // The command finishes into a vi-mode prompt.
        DaemonMsg::Prompt {
            active: true,
            at_prompt: true,
            last_exit: Some(0),
        }
        .encode(&mut daemon)
        .unwrap();
        DaemonMsg::Output(b"\x1b]133;V;1\x07\x1b]133;B\x07".to_vec())
            .encode(&mut daemon)
            .unwrap();
        for _ in 0..200 {
            cx.run_until_parked();
            let ready = window
                .update(cx, |view, _, _| {
                    view.terminal.shell_vi_mode() && view.terminal.zle_reading()
                })
                .unwrap();
            if ready {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        // Fire any pending hold-window timer too, so both release paths are
        // covered regardless of which one runs first.
        cx.executor().advance_clock(HOLD_WINDOW * 2);
        cx.run_until_parked();
        assert_eq!(
            next_input_until_timeout(&mut daemon),
            Some(b"ls".to_vec()),
            "gap text typed before a vi prompt must reach the shell"
        );

        // Back to an emacs-mode prompt: the editor re-engages empty-handed.
        DaemonMsg::Output(b"\x1b]133;V;0\x07\x1b]133;B\x07".to_vec())
            .encode(&mut daemon)
            .unwrap();
        for _ in 0..200 {
            cx.run_until_parked();
            let active = window.update(cx, |view, _, _| view.input_active()).unwrap();
            if active {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        window
            .update(cx, |view, _, _| {
                assert!(view.input_active());
                assert_eq!(
                    view.cmd.text(),
                    "",
                    "gap text consumed at the vi prompt must not resurrect in the editor"
                );
            })
            .unwrap();
        assert_eq!(
            next_input_until_timeout(&mut daemon),
            None,
            "no stale ^U wipe once the vi prompt consumed the gap text"
        );
    }

    fn key(spec: &str) -> gpui::Keystroke {
        gpui::Keystroke::parse(spec).expect("valid keystroke spec")
    }

    /// The notice names only known fig-style shims — an ordinary foreground
    /// command (`ssh`) must not be blamed for intercepting anything, and the
    /// generic message must not claim interception it can't prove.
    #[test]
    fn shim_detection_names_known_wrappers_only() {
        assert_eq!(known_pty_shim("zsh (kiro-cli-term)"), Some("kiro-cli-term"));
        assert_eq!(known_pty_shim("figterm"), Some("figterm"));
        assert_eq!(known_pty_shim("qterm"), Some("qterm"));
        assert_eq!(known_pty_shim("ssh"), None);
        assert_eq!(known_pty_shim("wezterm"), None);
        assert_eq!(known_pty_shim(""), None);
        assert!(integration_notice_message(Some("kiro-cli-term")).contains("kiro-cli-term"));
        assert!(!integration_notice_message(None).contains("intercepting"));
    }

    /// The Ctrl+R integration notice (#46), through the real key dispatcher:
    /// silent inside the startup grace window, raised once integration has had
    /// time to engage and never did, dismissed by the next keystroke, and
    /// one-shot per pane. The chord itself still reaches the PTY throughout
    /// (the shell's own reverse-i-search is the fallback).
    #[gpui::test]
    fn ctrl_r_without_integration_raises_the_notice_once(cx: &mut TestAppContext) {
        // `note_integration_gap` queries the daemon for the pane's foreground
        // process; pin the config dir to a scratch so the control connection
        // fails cleanly instead of reaching a real user daemon.
        crate::core::config::pin_test_config_dir();

        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, window, cx| {
                let ctrl_r = KeyDownEvent {
                    keystroke: key("ctrl-r"),
                    is_held: false,
                    prefer_character_input: false,
                };
                // Fresh pane: the shell may legitimately not have reported yet.
                view.on_key_down(&ctrl_r, window, cx);
                assert!(
                    view.integration_notice.is_none(),
                    "the grace window stays silent"
                );

                // Past the grace window with no OSC 133 ever seen → notice.
                view.created_at = std::time::Instant::now() - INTEGRATION_GRACE * 2;
                view.on_key_down(&ctrl_r, window, cx);
                assert!(
                    view.integration_notice.is_some(),
                    "Ctrl+R raises the notice"
                );
                cx.notify();
            })
            .unwrap();

        // Let the notified frame actually draw — a panic in the notice layout
        // fails the test here.
        cx.run_until_parked();
        window
            .update(cx, |view, window, cx| {
                assert!(
                    view.integration_notice.is_some(),
                    "the notice survives a real render pass"
                );

                // The next keystroke dismisses it; the latch keeps it one-shot.
                let ctrl_r = KeyDownEvent {
                    keystroke: key("ctrl-r"),
                    is_held: false,
                    prefer_character_input: false,
                };
                view.on_key_down(&ctrl_r, window, cx);
                assert!(
                    view.integration_notice.is_none(),
                    "a keystroke dismisses the notice"
                );
                view.on_key_down(&ctrl_r, window, cx);
                assert!(
                    view.integration_notice.is_none(),
                    "the notice is one-shot per pane"
                );
            })
            .unwrap();
    }

    /// The `InsertNewline` action puts a literal newline at the caret and leaves
    /// the line unsubmitted; a plain Enter then ships the whole multi-line
    /// buffer. Behaviour that used to be hardcoded on Shift+Enter (#182).
    #[gpui::test]
    fn insert_newline_action_extends_the_line_and_enter_submits_it(cx: &mut TestAppContext) {
        // `submit_command` defers a history-file record; pin the config dir to
        // the shared test scratch so nothing touches the real user history.
        let dir = std::env::temp_dir().join(format!("tty7-covtest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        crate::core::config::set_config_dir(dir);

        let (window, mut daemon) = harness(cx);
        prompt_ready(&window, cx, &mut daemon);

        window
            .update(cx, |view, _, cx| {
                assert!(view.input_active(), "the editor owns an idle prompt");
                view.commit_text("echo a", cx);
                view.insert_newline_action(cx);
                view.commit_text("echo b", cx);
                assert_eq!(view.cmd.text(), "echo a\necho b");

                view.handle_editor_key(&key("enter"), cx);
                assert!(view.cmd.is_empty(), "Enter submits the whole buffer");
            })
            .unwrap();
        assert_eq!(
            next_input_until_timeout(&mut daemon),
            // Submit sends each buffer line as a carriage return, the way a
            // pasted multi-line command already goes out.
            Some(b"echo a\recho b\r".to_vec()),
            "the multi-line command reaches the PTY in one submit"
        );
    }

    /// With a completion menu open the action still inserts, and closes the
    /// menu: the newline ends the word being completed, so a menu still
    /// filtered on the old word would be stale. Plain Enter keeps its own
    /// meaning there — it accepts the highlighted candidate (#182).
    #[gpui::test]
    fn insert_newline_action_closes_the_completion_menu_but_enter_still_accepts(
        cx: &mut TestAppContext,
    ) {
        let (window, mut daemon) = harness(cx);
        prompt_ready(&window, cx, &mut daemon);

        let candidate = |text: &str| completion::Candidate {
            text: text.to_string(),
            kind: CandidateKind::Command,
            start: 4,
            end: 4,
            description: None,
            icon: None,
        };

        window
            .update(cx, |view, _, cx| {
                // Menu open on the word after "git ".
                view.cmd.set_with_cursor("git ", 4);
                view.open_completion(CompletionSession::new(
                    4,
                    String::new(),
                    vec![candidate("status")],
                ));

                view.insert_newline_action(cx);
                assert!(
                    view.completion.is_none(),
                    "the newline ends the completed word, so the menu closes"
                );
                assert_eq!(view.cmd.text(), "git \n");

                // Plain Enter with a menu open is a different gesture: it takes
                // the highlighted candidate rather than submitting or inserting.
                view.cmd.set_with_cursor("git ", 4);
                view.open_completion(CompletionSession::new(
                    4,
                    String::new(),
                    vec![candidate("status")],
                ));
                view.handle_editor_key(&key("enter"), cx);
                // Accepting a command candidate leaves the trailing space that
                // starts the next word.
                assert_eq!(view.cmd.text(), "git status ");
            })
            .unwrap();
    }

    /// The action is the prompt editor's alone: with a foreground application on
    /// the alternate screen it declines, so the chord takes its old path out to
    /// the application instead of editing a line that isn't there.
    #[gpui::test]
    fn insert_newline_action_declines_when_the_editor_is_not_live(cx: &mut TestAppContext) {
        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, _, cx| {
                view.cmd.set("keep me");
                view.terminal.exited = true; // simplest input_active() = false
                assert!(!view.input_active());
                view.insert_newline_action(cx);
                assert_eq!(view.cmd.text(), "keep me", "no newline inserted");
            })
            .unwrap();
    }

    /// The check the tests above structurally can't make: with the *real* keymap
    /// installed, both default chords have to actually reach the action. They
    /// call `insert_newline_action` directly, so a wrong key context — or a
    /// `NoAction` from a later `rebind` shadowing the chord — would leave every
    /// one of them green while Shift+Enter silently submitted the line. This
    /// drives the keystroke through GPUI's dispatch instead (#182).
    #[gpui::test]
    fn the_keymap_routes_both_newline_chords_to_the_action(cx: &mut TestAppContext) {
        let (window, mut daemon) = harness(cx);
        cx.update(|cx| crate::ui::keymap::init(cx));
        prompt_ready(&window, cx, &mut daemon);
        window
            .update(cx, |view, window, cx| {
                window.activate_window();
                view.focus_handle.focus(window, cx);
                view.commit_text("echo a", cx);
            })
            .unwrap();

        let mut vcx = gpui::VisualTestContext::from_window(window.into(), cx);
        vcx.simulate_keystrokes("shift-enter");
        vcx.simulate_keystrokes("alt-enter");
        window
            .update(cx, |view, _, _| {
                assert_eq!(
                    view.cmd.text(),
                    "echo a\n\n",
                    "both chords dispatched InsertNewline instead of submitting"
                );
            })
            .unwrap();

        // And again after a rebind, which is when the suppression bindings go in:
        // the `NoAction` retiring the old chord must not outrank the identical
        // one being re-installed alongside it.
        cx.update(|cx| crate::ui::keymap::rebind(cx));
        vcx.simulate_keystrokes("shift-enter");
        window
            .update(cx, |view, _, _| {
                assert_eq!(
                    view.cmd.text(),
                    "echo a\n\n\n",
                    "the chord survives a rebind"
                );
            })
            .unwrap();
    }

    /// The Ctrl+R flow end-to-end at the editor dispatcher: Ctrl+R opens the
    /// search, typed text (the IME/commit path) edits the query with fuzzy
    /// matching, Enter loads the selection into the editor without running it.
    #[gpui::test]
    fn ctrl_r_fuzzy_search_accepts_into_the_editor(cx: &mut TestAppContext) {
        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, _, cx| {
                view.history = ["git status", "cargo build", "git commit -m x"]
                    .into_iter()
                    .map(String::from)
                    .collect();
                view.history_frecency = vec![0.0; view.history.len()];

                view.handle_editor_key(&key("ctrl-r"), cx);
                assert!(view.reverse_search.is_some(), "Ctrl+R opens the search");
                // `gst` is a subsequence of `git status` — fuzzy, not substring.
                view.commit_text("gst", cx);
                assert_eq!(
                    view.reverse_search
                        .as_ref()
                        .and_then(|rs| rs.selected_line(&view.history)),
                    Some("git status")
                );
                view.handle_editor_key(&key("enter"), cx);
                assert!(view.reverse_search.is_none(), "Enter closes the search");
                assert_eq!(view.cmd.text(), "git status");
            })
            .unwrap();
    }

    /// Repeated Ctrl+R steps down the ranked matches, and Cmd+Enter runs the
    /// selection outright: the line must come out of the client socket as
    /// `Input` bytes ending in `\r`.
    #[gpui::test]
    fn ctrl_r_steps_matches_and_cmd_enter_runs(cx: &mut TestAppContext) {
        // `submit_command` defers a history-file record; pin the config dir to
        // the shared test scratch so nothing touches the real user history.
        crate::core::config::pin_test_config_dir();

        let (window, mut daemon) = harness(cx);
        window
            .update(cx, |view, _, cx| {
                view.history = ["git status", "cargo build", "git commit -m x"]
                    .into_iter()
                    .map(String::from)
                    .collect();
                view.history_frecency = vec![0.0; view.history.len()];

                view.handle_editor_key(&key("ctrl-r"), cx);
                view.commit_text("git", cx);
                // Equal fuzzy scores: the newer entry ranks first; a second
                // Ctrl+R steps to the older match.
                assert_eq!(
                    view.reverse_search
                        .as_ref()
                        .and_then(|rs| rs.selected_line(&view.history)),
                    Some("git commit -m x")
                );
                view.handle_editor_key(&key("ctrl-r"), cx);
                assert_eq!(
                    view.reverse_search
                        .as_ref()
                        .and_then(|rs| rs.selected_line(&view.history)),
                    Some("git status")
                );
                view.handle_editor_key(&key("cmd-enter"), cx);
                assert!(view.reverse_search.is_none());
                assert!(view.cmd.is_empty(), "submit clears the editor");
            })
            .unwrap();
        assert_eq!(
            next_input_until_timeout(&mut daemon),
            Some(b"git status\r".to_vec()),
            "Cmd+Enter ships the selected line to the PTY"
        );
    }

    /// Ctrl+J and Ctrl+M are accept-line's control codes, so at the prompt they
    /// must submit exactly as Enter does (#163) — before the fix they fell into
    /// `apply_readline_ctrl`'s no-op arm and the key did nothing at all.
    #[gpui::test]
    fn ctrl_j_and_ctrl_m_submit_the_line_like_enter(cx: &mut TestAppContext) {
        // `submit_command` defers a history-file record; pin the config dir to
        // the shared test scratch so nothing touches the real user history.
        crate::core::config::pin_test_config_dir();

        let (window, mut daemon) = harness(cx);
        for (chord, line) in [("ctrl-j", "echo j"), ("ctrl-m", "echo m")] {
            window
                .update(cx, |view, _, cx| {
                    view.cmd.set(line);
                    view.handle_editor_key(&key(chord), cx);
                    assert!(view.cmd.is_empty(), "{chord} clears the editor");
                })
                .unwrap();
            assert_eq!(
                next_input_until_timeout(&mut daemon),
                Some(format!("{line}\r").into_bytes()),
                "{chord} ships the line to the PTY"
            );
        }
    }

    /// With `history_search` off, Ctrl+R never opens tty7's menu: the edited
    /// line is handed to the shell and the raw `^R` follows it, so a user's own
    /// binding there (fzf, percol, plain reverse-i-search) answers (#163).
    #[gpui::test]
    fn history_search_off_sends_ctrl_r_to_the_shell(cx: &mut TestAppContext) {
        let (window, mut daemon) = harness(cx);
        cx.update(|cx| {
            let mut cfg = cx.global::<Config>().clone();
            cfg.history_search = false;
            cx.set_global(cfg);
        });
        window
            .update(cx, |view, _, cx| {
                view.history = ["git status"].into_iter().map(String::from).collect();
                view.history_frecency = vec![0.0; view.history.len()];
                view.cmd.set("gi");
                view.handle_editor_key(&key("ctrl-r"), cx);
                assert!(
                    view.reverse_search.is_none(),
                    "no tty7 menu while opted out"
                );
                assert_eq!(view.cmd.text(), "", "the line went to the shell");
            })
            .unwrap();
        assert_eq!(next_input_until_timeout(&mut daemon), Some(b"gi".to_vec()));
        assert_eq!(
            next_input_until_timeout(&mut daemon),
            Some(vec![0x12]),
            "the raw ^R follows the handed-over line"
        );
    }

    /// The Ctrl+R menu actually renders while the shell sits at its prompt:
    /// with `input_active` true and a search open over entries carrying run
    /// metadata, a real (headless) frame draws `render_reverse_search_menu` —
    /// guarding the row/highlight/badge layout code against panics that unit
    /// tests of the search logic can't reach.
    #[gpui::test]
    fn reverse_search_menu_survives_a_real_render_pass(cx: &mut TestAppContext) {
        let (window, mut daemon) = harness(cx);
        // Put the shell at its prompt so `input_active()` is true and the
        // menu branch of `render` runs.
        DaemonMsg::Prompt {
            active: true,
            at_prompt: true,
            last_exit: None,
        }
        .encode(&mut daemon)
        .unwrap();
        for _ in 0..200 {
            if window
                .update(cx, |view, _, _| view.terminal.at_prompt())
                .unwrap()
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        window
            .update(cx, |view, _, cx| {
                assert!(view.input_active(), "prompt report engages the editor");
                view.history = ["git status", "cargo build --release", "echo hello"]
                    .into_iter()
                    .map(String::from)
                    .collect();
                view.history_frecency = vec![0.0; view.history.len()];
                // Metadata for the badge/ago column: one failed run, one aged.
                view.history_meta.insert(
                    "cargo build --release".into(),
                    super::super::history::EntryMeta {
                        ts: Some(unix_now().saturating_sub(7200)),
                        exit: Some(1),
                    },
                );
                view.handle_editor_key(&key("ctrl-r"), cx);
                view.commit_text("c", cx);
                assert!(
                    view.reverse_search
                        .as_ref()
                        .is_some_and(|rs| !rs.matches().is_empty()),
                    "the query has matches for the menu to draw"
                );
                cx.notify();
            })
            .unwrap();
        // Let the notified frame actually draw — a panic in the menu layout
        // or row rendering fails the test here.
        cx.run_until_parked();
        window
            .update(cx, |view, _, _| {
                assert!(view.reverse_search.is_some(), "search survives the frame");
            })
            .unwrap();
    }

    /// The deferred history record picks up the command's exit code once the
    /// shell reports back at its prompt (OSC 133;D → daemon `Prompt` frame →
    /// `prompt_seq`/`last_exit_code`), and the file line carries it.
    #[gpui::test]
    fn submitted_command_backfills_its_exit_code(cx: &mut TestAppContext) {
        // `set_config_dir` is first-call-wins and process-wide, so this pin only
        // takes if no other test got there first — read the *effective* dir back
        // rather than assuming this one won.
        crate::core::config::pin_test_config_dir();
        let dir = crate::core::config::config_dir_path().expect("a config dir resolves");

        let (window, mut daemon) = harness(cx);
        let wait = |cx: &mut TestAppContext, pred: &dyn Fn(&TerminalView) -> bool, what: &str| {
            for _ in 0..200 {
                if window.update(cx, |view, _, _| pred(view)).unwrap() {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            panic!("timed out waiting for {what}");
        };

        // The shell reaches its prompt (integration active).
        DaemonMsg::Prompt {
            active: true,
            at_prompt: true,
            last_exit: None,
        }
        .encode(&mut daemon)
        .unwrap();
        wait(cx, &|v| v.terminal.at_prompt(), "the initial prompt report");

        let marker = format!("tty7_gpui_exit_marker_{}", std::process::id());
        window
            .update(cx, |view, _, cx| {
                view.cmd.set(&marker);
                view.submit_command(cx);
                assert!(view.pending_history.is_some(), "record defers for the exit");
            })
            .unwrap();

        // The command runs (leaves the prompt) and finishes with exit 3.
        DaemonMsg::Prompt {
            active: true,
            at_prompt: false,
            last_exit: None,
        }
        .encode(&mut daemon)
        .unwrap();
        DaemonMsg::Prompt {
            active: true,
            at_prompt: true,
            last_exit: Some(3),
        }
        .encode(&mut daemon)
        .unwrap();
        wait(
            cx,
            &|v| v.terminal.at_prompt() && v.terminal.last_exit_code() == Some(3),
            "the post-command prompt report",
        );

        window
            .update(cx, |view, window, cx| {
                view.poll_foreground(window, cx);
                assert!(view.pending_history.is_none(), "poll flushed the record");
                assert_eq!(
                    view.history_meta.get(&marker).and_then(|m| m.exit),
                    Some(3),
                    "in-memory metadata learned the exit code"
                );
            })
            .unwrap();

        // The file record is the current format with the exit code attached.
        let content = std::fs::read_to_string(dir.join("history")).expect("history file written");
        let line = content
            .lines()
            .find(|l| l.contains(&marker))
            .expect("the submitted command was recorded");
        let mut fields = line.splitn(4, '\t');
        let ts = fields.next().unwrap();
        assert!(!ts.is_empty() && ts.bytes().all(|b| b.is_ascii_digit()));
        assert_eq!(fields.next(), Some("3"), "exit code field");
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
                // Other Meta letters have no local widget, so they hand the line
                // to the shell rather than dying here — see
                // `an_unknown_meta_chord_goes_to_the_shell_with_the_line`.
                view.handle_editor_key(&meta("z"), cx);
                assert_eq!(view.cmd.text(), "");
            })
            .unwrap();
    }

    /// Fill the scrollback and park the viewport `offset` lines up inside it,
    /// so a test can watch a keystroke snap it back to the live prompt.
    fn scroll_into_history(view: &TerminalView, offset: usize) {
        let mut parser: alacritty_terminal::vte::ansi::Processor = Default::default();
        let mut term = view.terminal.term.lock();
        parser.advance(&mut *term, &b"line\r\n".repeat(60));
        term.scroll_display(Scroll::Delta(offset as i32));
        assert_eq!(
            term.grid().display_offset(),
            offset,
            "the viewport starts parked in the scrollback"
        );
    }

    fn display_offset(view: &TerminalView) -> usize {
        view.terminal.term.lock().grid().display_offset()
    }

    /// Scrolled up into the scrollback, recalling history with ↑ must bring the
    /// viewport back to the live prompt (#208). The local editor owns ↑ and
    /// returns early, so it never reached the "typing jumps to the prompt"
    /// housekeeping on the raw key path — leaving the user editing a line they
    /// cannot see.
    #[gpui::test]
    fn history_recall_snaps_the_viewport_back_to_the_prompt(cx: &mut TestAppContext) {
        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, _, cx| {
                view.history = vec!["echo hello".to_string()];
                scroll_into_history(view, 10);
                view.scroll_frac = 0.5;

                view.handle_editor_key(&key("up"), cx);

                assert_eq!(view.cmd.text(), "echo hello", "↑ recalled the entry");
                assert_eq!(display_offset(view), 0, "and the viewport followed it down");
                assert_eq!(view.scroll_frac, 0., "the sub-line remainder reset too");
            })
            .unwrap();
    }

    /// ⌃P / ⌃N are readline's history motions, and a raw terminal passes them
    /// to the shell as 0x10 / 0x0e. The local editor swallows every Ctrl chord
    /// at the prompt, so without arms of their own they went from "works" to
    /// "does nothing" the moment shell integration engaged.
    #[gpui::test]
    fn ctrl_p_and_ctrl_n_walk_the_history(cx: &mut TestAppContext) {
        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, _, cx| {
                view.history = ["git status", "cargo build", "echo hello"]
                    .into_iter()
                    .map(String::from)
                    .collect();

                // ⌃P walks back from the newest entry.
                view.handle_editor_key(&key("ctrl-p"), cx);
                assert_eq!(view.cmd.text(), "echo hello");
                view.handle_editor_key(&key("ctrl-p"), cx);
                assert_eq!(view.cmd.text(), "cargo build");
                // ⌃N walks forward again.
                view.handle_editor_key(&key("ctrl-n"), cx);
                assert_eq!(view.cmd.text(), "echo hello");
                // Past the newest entry the in-progress line comes back.
                view.handle_editor_key(&key("ctrl-n"), cx);
                assert_eq!(view.cmd.text(), "");
            })
            .unwrap();
    }

    /// A Ctrl chord the local editor has no widget for used to be swallowed, so
    /// engaging shell integration *removed* ⌃T, ⌥T, ⌥U and every `bindkey`
    /// widget the user had bound. Hand the line to zle instead and let its
    /// keymap answer — the same escape hatch ⌃R already uses.
    #[gpui::test]
    fn an_unknown_ctrl_chord_goes_to_the_shell_with_the_line(cx: &mut TestAppContext) {
        let (window, mut daemon) = harness(cx);
        window
            .update(cx, |view, _, cx| {
                view.cmd.set("echo hi");
                // ⌃T is readline's transpose-chars; tty7 has no widget for it.
                view.handle_editor_key(&key("ctrl-t"), cx);
                assert_eq!(
                    view.cmd.text(),
                    "",
                    "the line left for the shell, so the local buffer is empty"
                );
                assert!(
                    view.editor_handoff.is_some(),
                    "the local editor stands down for the rest of the line"
                );
            })
            .unwrap();
        assert_eq!(next_input(&mut daemon), b"echo hi".to_vec());
        assert_eq!(next_input(&mut daemon), vec![0x14], "⌃T reached the shell");
    }

    /// The Meta half of the same gap: ⌥U (upcase-word) and friends were dead at
    /// the prompt. Unrecognized Meta chords ship the line and the ESC-prefixed
    /// key, the way a raw terminal would have.
    #[gpui::test]
    fn an_unknown_meta_chord_goes_to_the_shell_with_the_line(cx: &mut TestAppContext) {
        let (window, mut daemon) = harness(cx);
        window
            .update(cx, |view, _, cx| {
                view.cmd.set("echo hi");
                view.handle_editor_key(
                    &gpui::Keystroke {
                        modifiers: gpui::Modifiers {
                            alt: true,
                            ..Default::default()
                        },
                        key: "u".to_string(),
                        key_char: None,
                    },
                    cx,
                );
                assert_eq!(view.cmd.text(), "");
            })
            .unwrap();
        assert_eq!(next_input(&mut daemon), b"echo hi".to_vec());
        assert_eq!(next_input(&mut daemon), b"\x1bu".to_vec());
    }

    /// ⌃W / ⌃U / ⌃K are *kills*, and ⌃Y is what puts a kill back — without it
    /// the pair was half-implemented: the editor cut text with nowhere to paste
    /// it from. ⌃Y has to stay local rather than reaching the shell, because
    /// zle's kill ring is a different buffer and would yank unrelated text.
    #[gpui::test]
    fn ctrl_y_yanks_back_what_the_kill_chords_cut(cx: &mut TestAppContext) {
        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, _, cx| {
                view.cmd.set("echo hello world");
                view.handle_editor_key(&key("ctrl-w"), cx);
                assert_eq!(view.cmd.text(), "echo hello ");
                view.handle_editor_key(&key("ctrl-y"), cx);
                assert_eq!(view.cmd.text(), "echo hello world");
                assert!(
                    view.editor_handoff.is_none(),
                    "the line never left for the shell"
                );
            })
            .unwrap();
    }

    /// ⌥. is readline's `yank-last-arg`: it pulls the last word of the previous
    /// command into the line, and repeating it walks further back through the
    /// history, replacing what the last press inserted. Frequent enough that
    /// paying the handoff cost (ghost text and completion gone for the rest of
    /// the line) on every press would be the wrong trade — tty7 holds the same
    /// history, so it answers locally.
    #[gpui::test]
    fn meta_dot_walks_back_through_the_last_words(cx: &mut TestAppContext) {
        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, _, cx| {
                let meta_dot = gpui::Keystroke {
                    modifiers: gpui::Modifiers {
                        alt: true,
                        ..Default::default()
                    },
                    key: ".".to_string(),
                    key_char: None,
                };
                view.history = ["git status", "cargo build --release", "echo hello world"]
                    .into_iter()
                    .map(String::from)
                    .collect();
                view.cmd.set("ls ");

                view.handle_editor_key(&meta_dot, cx);
                assert_eq!(view.cmd.text(), "ls world", "newest entry's last word");
                view.handle_editor_key(&meta_dot, cx);
                assert_eq!(view.cmd.text(), "ls --release", "repeat steps one back");
                view.handle_editor_key(&meta_dot, cx);
                assert_eq!(view.cmd.text(), "ls status");
                // Nothing older to reach: the line holds what it had.
                view.handle_editor_key(&meta_dot, cx);
                assert_eq!(view.cmd.text(), "ls status");
                // The caret sits after the inserted word, ready to keep typing.
                assert_eq!(view.cmd.cursor(), "ls status".chars().count());
            })
            .unwrap();
    }

    /// The walk is only a walk while ⌥. repeats. Once another key edits the
    /// line, the next ⌥. starts over from the newest entry instead of eating
    /// whatever happens to sit left of the caret.
    #[gpui::test]
    fn an_intervening_key_restarts_the_last_word_walk(cx: &mut TestAppContext) {
        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, _, cx| {
                let meta_dot = gpui::Keystroke {
                    modifiers: gpui::Modifiers {
                        alt: true,
                        ..Default::default()
                    },
                    key: ".".to_string(),
                    key_char: None,
                };
                view.history = ["cargo build --release", "echo hello world"]
                    .into_iter()
                    .map(String::from)
                    .collect();

                view.handle_editor_key(&meta_dot, cx);
                assert_eq!(view.cmd.text(), "world");
                view.handle_editor_key(&key("left"), cx);
                view.handle_editor_key(&key("end"), cx);
                view.handle_editor_key(&meta_dot, cx);
                assert_eq!(
                    view.cmd.text(),
                    "worldworld",
                    "a fresh walk appends rather than replacing the earlier word"
                );
            })
            .unwrap();
    }

    /// Edits that bypass `handle_editor_key` — IME-committed text is the
    /// everyday one (it's how all typing arrives on macOS and Windows) — must
    /// end the walk too. Without that, the next ⌥. deletes the span the walk
    /// recorded even though the user's typing now sits inside it.
    #[gpui::test]
    fn an_intervening_ime_commit_restarts_the_last_word_walk(cx: &mut TestAppContext) {
        let (window, mut daemon) = harness(cx);
        // `commit_text` edits the local line only while the editor is engaged
        // at a shell prompt; anywhere else it writes gap text to the PTY.
        DaemonMsg::Prompt {
            active: true,
            at_prompt: true,
            last_exit: None,
        }
        .encode(&mut daemon)
        .unwrap();
        wait_for_input_active(&window, cx);
        window
            .update(cx, |view, _, cx| {
                let meta_dot = gpui::Keystroke {
                    modifiers: gpui::Modifiers {
                        alt: true,
                        ..Default::default()
                    },
                    key: ".".to_string(),
                    key_char: None,
                };
                view.history = ["cargo build --release", "echo hello world"]
                    .into_iter()
                    .map(String::from)
                    .collect();

                view.handle_editor_key(&meta_dot, cx);
                assert_eq!(view.cmd.text(), "world");
                view.commit_text("x", cx);
                view.handle_editor_key(&meta_dot, cx);
                assert_eq!(
                    view.cmd.text(),
                    "worldxworld",
                    "the typed char survives; the walk starts over after it"
                );
            })
            .unwrap();
    }

    /// ⌥. with a selection active: the word replaces the selection (insertion
    /// replaces selections everywhere in this editor), and the walk records
    /// where the word actually landed — the caret the selection collapsed to,
    /// not where the caret stood before the insert — so a repeat swaps the
    /// word cleanly.
    #[gpui::test]
    fn meta_dot_over_a_selection_records_where_the_word_landed(cx: &mut TestAppContext) {
        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, _, cx| {
                let meta_dot = gpui::Keystroke {
                    modifiers: gpui::Modifiers {
                        alt: true,
                        ..Default::default()
                    },
                    key: ".".to_string(),
                    key_char: None,
                };
                view.history = ["cargo build --release", "echo hello world"]
                    .into_iter()
                    .map(String::from)
                    .collect();
                view.cmd.set("ls foo");
                // Select "foo" with the caret at the selection's far end.
                view.cmd.set_cursor(3);
                view.cmd.extend_to(6);

                view.handle_editor_key(&meta_dot, cx);
                assert_eq!(
                    view.cmd.text(),
                    "ls world",
                    "the word replaced the selection"
                );
                view.handle_editor_key(&meta_dot, cx);
                assert_eq!(
                    view.cmd.text(),
                    "ls --release",
                    "the repeat swapped the word, not some other span"
                );
            })
            .unwrap();
    }

    /// A shifted Meta chord must ship the shifted character: ⌥⇧U is `ESC U`
    /// on the wire (upcase-region in zsh's keymap), not the `ESC u` of plain
    /// ⌥U — gpui reports the key name unshifted, so the handoff has to apply
    /// Shift itself when no `key_char` is there to consult.
    #[gpui::test]
    fn a_shifted_meta_chord_hands_off_the_shifted_character(cx: &mut TestAppContext) {
        let (window, mut daemon) = harness(cx);
        window
            .update(cx, |view, _, cx| {
                view.cmd.set("echo hi");
                view.handle_editor_key(
                    &gpui::Keystroke {
                        modifiers: gpui::Modifiers {
                            alt: true,
                            shift: true,
                            ..Default::default()
                        },
                        key: "u".to_string(),
                        key_char: None,
                    },
                    cx,
                );
                assert_eq!(view.cmd.text(), "");
            })
            .unwrap();
        assert_eq!(next_input(&mut daemon), b"echo hi".to_vec());
        assert_eq!(next_input(&mut daemon), b"\x1bU".to_vec());
    }

    /// Chords the editor *does* answer stay local — handing off would forfeit
    /// ghost text and completion for the rest of the line, and ⌃A/⌃E/⌃W are
    /// exactly the keys pressed most often mid-edit.
    #[gpui::test]
    fn a_known_ctrl_chord_stays_in_the_local_editor(cx: &mut TestAppContext) {
        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, _, cx| {
                view.cmd.set("echo hi");
                view.handle_editor_key(&key("ctrl-w"), cx);
                assert_eq!(view.cmd.text(), "echo ", "⌃W cut the word locally");
                assert!(view.editor_handoff.is_none());
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

    // ── The read-only degrade, at the five keystroke entry points ───

    /// Install a store holding one remote workspace with no connection, and
    /// bind `view` to it. `RemoteLinks` has never heard of the machine, so
    /// `status_of` answers `Disconnected` — the state a window sits in between
    /// losing a link and getting it back.
    fn bind_to_a_disconnected_remote_workspace(
        view: &mut TerminalView,
        cx: &mut Context<TerminalView>,
    ) -> crate::core::session::WorkspaceId {
        use crate::core::session::{
            RemoteRef, RemoteTarget, WorkspaceId, WorkspaceStore, Workspaces,
        };
        use crate::terminal::PaneWorkspace;
        let host = RemoteRef::new(
            RemoteTarget::direct("me", "build-box", 22),
            WorkspaceId::new(),
        );
        let entry = crate::core::session::Workspace::on_remote(host.clone());
        let id = entry.id;
        WorkspaceStore::install_for_test(
            cx,
            Workspaces {
                workspaces: vec![entry],
                active: None,
            },
        );
        view.set_workspace(Some(PaneWorkspace {
            workspace: id,
            target: host.target,
            spec: Some(Box::new(
                serde_json::from_str(
                    r#"{"host":"build-box","port":22,"user":"me","auth_mode":"auto"}"#,
                )
                .unwrap(),
            )),
        }));
        id
    }

    /// A window that is not attached **still shows, scrolls,
    /// selects and searches — but typing goes nowhere**, and nothing is
    /// buffered for later (D6).
    ///
    /// All five entry points a keystroke can take, because a rule enforced at
    /// four of them is not enforced: the one that is missed is the one a user
    /// finds.
    #[gpui::test]
    fn a_disconnected_remote_pane_swallows_every_kind_of_typing(cx: &mut TestAppContext) {
        let (window, mut daemon) = harness(cx);
        window
            .update(cx, |view, window, cx| {
                bind_to_a_disconnected_remote_workspace(view, cx);

                type_char(view, "x", window, cx);
                view.commit_text("y", cx);
                view.paste("pasted".into(), cx);
                view.send_to_pty(b"raw", cx);
                // The typeahead timer: armed while attached, fires afterwards.
                view.dump_hold(0, cx);
            })
            .unwrap();
        assert_eq!(
            next_input_until_timeout(&mut daemon),
            None,
            "a read-only window must not put one byte of typing on the wire"
        );
    }

    /// The rest of the degrade, and the half a gate at the top of
    /// `on_key_down` would silently destroy: **a read-only window is not an
    /// inert one.**
    ///
    /// "能滚历史、能选能复制、能 ⌘F 搜索" — the window's own keyboard belongs to
    /// the window, not to the machine. ⌘A and ⌘C are dispatched *inside*
    /// `on_key_down` (`handle_cmd_shortcut`), so a gate at the top of that
    /// function takes them away; this drives the real dispatcher to prove it
    /// does not. (⌘F is a registered action and never enters `on_key_down` at
    /// all, which is why it survives either placement — and why testing only
    /// ⌘F would have missed this entirely.)
    #[gpui::test]
    fn a_disconnected_remote_pane_still_selects_and_copies(cx: &mut TestAppContext) {
        let (window, mut daemon) = harness(cx);
        DaemonMsg::Output(b"secrets".to_vec())
            .encode(&mut daemon)
            .unwrap();
        for _ in 0..400 {
            cx.run_until_parked();
            let ready = window
                .update(cx, |view, _, _| {
                    let term = view.terminal.term.clone();
                    let term = term.lock();
                    term.grid()[alacritty_terminal::index::Line(0)][Column(0)].c == 's'
                })
                .unwrap();
            if ready {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        let chord = |key: &str| KeyDownEvent {
            keystroke: gpui::Keystroke {
                modifiers: Modifiers {
                    platform: true,
                    ..Modifiers::default()
                },
                key: key.into(),
                key_char: None,
            },
            is_held: false,
            prefer_character_input: false,
        };
        window
            .update(cx, |view, window, cx| {
                bind_to_a_disconnected_remote_workspace(view, cx);
                // A dropped link marks the pane `exited` (the reader's teardown
                // sets the same flag a real child exit does). That must not take
                // the window's own keyboard away.
                view.terminal.exited = true;
                view.on_key_down(&chord("a"), window, cx);
                assert!(
                    view.terminal.term.lock().selection.is_some(),
                    "⌘A must still select on a read-only window"
                );
                view.on_key_down(&chord("c"), window, cx);
            })
            .unwrap();
        let copied = cx.update(|cx| cx.read_from_clipboard().and_then(|item| item.text()));
        assert!(
            copied.is_some_and(|t| t.contains("secrets")),
            "⌘C must still copy on a read-only window"
        );
    }

    /// The other half of the same rule, and the one that is easy to get wrong:
    /// a **terminal query reply is not user input**.
    ///
    /// DA / DSR / OSC colour answers are the emulator replying to something the
    /// *remote program* asked. Gating them would not degrade the window, it
    /// would hang the program — it waits for an answer that never comes.
    #[gpui::test]
    fn a_disconnected_remote_pane_still_answers_terminal_queries(cx: &mut TestAppContext) {
        let (window, mut daemon) = harness(cx);
        window
            .update(cx, |view, _, cx| {
                bind_to_a_disconnected_remote_workspace(view, cx);
                view.handle_event(AlacEvent::PtyWrite("\x1b[?62;c".into()), cx);
            })
            .unwrap();
        assert_eq!(
            next_input(&mut daemon),
            b"\x1b[?62;c".to_vec(),
            "a query reply is the emulator's answer, not the user's typing"
        );
    }

    /// A dropped link and a finished shell are opposite facts, and the tab must
    /// not confuse them: on a remote workspace the shell is still running over
    /// there, which is the entire promise of the degrade.
    #[gpui::test]
    fn a_dropped_link_does_not_claim_the_process_exited(cx: &mut TestAppContext) {
        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, _, cx| {
                bind_to_a_disconnected_remote_workspace(view, cx);
                view.handle_event(AlacEvent::Exit, cx);
                assert_eq!(view.title, "tty7 — disconnected");

                // A local pane's wording is untouched.
                view.set_workspace(None);
                view.handle_event(AlacEvent::Exit, cx);
                assert_eq!(view.title, "tty7 — process exited");
            })
            .unwrap();
    }

    /// The other side of that exemption: **a local pane's exited check is not
    /// touched.** A pane whose shell ended still swallows every key exactly as
    /// it did before remote workspaces existed.
    #[gpui::test]
    fn an_exited_local_pane_still_swallows_every_key(cx: &mut TestAppContext) {
        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, window, cx| {
                view.terminal.exited = true;
                let cmd_a = KeyDownEvent {
                    keystroke: gpui::Keystroke {
                        modifiers: Modifiers {
                            platform: true,
                            ..Modifiers::default()
                        },
                        key: "a".into(),
                        key_char: None,
                    },
                    is_held: false,
                    prefer_character_input: false,
                };
                view.on_key_down(&cmd_a, window, cx);
                assert!(
                    view.terminal.term.lock().selection.is_none(),
                    "an exited local pane is finished; its keyboard is unchanged"
                );
            })
            .unwrap();
    }

    /// **A local pane is not gated, ever.** It has no connection to lose, and a
    /// gate that could answer `false` for one would brick the app — so the
    /// check is a field test on `workspace()`, and this pins that it stays one.
    #[gpui::test]
    fn a_local_pane_types_exactly_as_it_always_did(cx: &mut TestAppContext) {
        let (window, mut daemon) = harness(cx);
        window
            .update(cx, |view, window, cx| {
                // Even with a store installed that knows about a *remote*
                // workspace — the global existing must not change a local pane.
                bind_to_a_disconnected_remote_workspace(view, cx);
                view.set_workspace(None);
                assert!(view.accepts_input(cx));
                type_char(view, "z", window, cx);
            })
            .unwrap();
        assert_eq!(next_input(&mut daemon), b"z".to_vec());
    }

    // ── The reconnect: the pane relink ──────────────────────────────

    /// The pane half of a reconnect swaps the socket **in place**: same `Term`,
    /// same event channel, same shared signals — because the view's event pump
    /// subscribes once, at construction, and a fresh terminal would leave the
    /// pane on screen and permanently deaf.
    ///
    /// Also pins the honest replay boundary: the mirror is reset, so what is on
    /// screen after a relink is the machine's own record and not the pre-drop
    /// screen with a second copy replayed underneath it.
    #[gpui::test]
    fn a_relink_moves_the_pane_onto_the_new_socket_and_resets_the_mirror(cx: &mut TestAppContext) {
        let (window, mut old_daemon) = harness(cx);
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

        // Something on screen from before the drop, through the real reader.
        DaemonMsg::Output(b"before".to_vec())
            .encode(&mut old_daemon)
            .unwrap();
        let mut seen = String::new();
        for _ in 0..400 {
            cx.run_until_parked();
            seen = read_row(cx, 6);
            if seen == "before" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(seen, "before", "the pre-drop screen is what we relink over");

        let (new_client, mut new_daemon) = UnixStream::pair().unwrap();
        window
            .update(cx, |view, _, cx| {
                view.adopt_relink(
                    new_client,
                    &crate::terminal::PaneRoute::Local,
                    TermSize::new(100, 30),
                    8,
                    17,
                    cx,
                )
                .expect("the swap itself cannot fail");
                assert_eq!(
                    view.title, "tty7",
                    "a relinked pane is not \"process exited\""
                );
            })
            .unwrap();
        // The pre-drop screen is gone rather than doubled: whatever the daemon
        // replays next is the whole truth, and the part the ring dropped is
        // simply absent — not interpolated, not implied to be coming.
        assert_ne!(
            read_row(cx, 6),
            "before",
            "the mirror must be reset before the daemon replays onto it"
        );

        // The last step: resize to *this* client's geometry.
        let resize = loop {
            match ClientMsg::read(&mut new_daemon).expect("the new socket is live") {
                ClientMsg::Resize(win) => break win,
                _ => continue,
            }
        };
        assert_eq!((resize.cols, resize.rows), (100, 30));

        // And input now goes to the new machine, not the dead one.
        window
            .update(cx, |view, _, cx| view.send_to_pty(b"after", cx))
            .unwrap();
        assert_eq!(next_input(&mut new_daemon), b"after".to_vec());

        // The retired socket is *closed*, not merely unused: reading it runs
        // out. That is what ends the old reader thread, and it is why a relink
        // cannot leave two readers racing to feed one grid.
        let mut leftovers: Vec<Vec<u8>> = Vec::new();
        loop {
            match ClientMsg::read(&mut old_daemon) {
                Ok(ClientMsg::Input(bytes)) => leftovers.push(bytes),
                Ok(_) => continue,
                Err(_) => break,
            }
        }
        assert!(
            leftovers.is_empty(),
            "the retired socket must never see another byte: {leftovers:?}"
        );
    }

    /// Buffer search (Cmd+F) end-to-end: the case ("Aa") and regex (".*")
    /// toggles change the match set, a broken regex flags an error instead of a
    /// silent zero-match, and closing persists the query. Drives the real
    /// `open_search` / `recompute_matches` / `close_search` path against a grid
    /// seeded through the reader thread.
    #[gpui::test]
    fn buffer_search_honors_case_and_regex_toggles(cx: &mut TestAppContext) {
        let (window, mut daemon) = harness(cx);

        // Three lines differing only by case, so the case toggle is observable.
        DaemonMsg::Output(b"Hello World\r\nhello world\r\nWORLD wide\r\n".to_vec())
            .encode(&mut daemon)
            .unwrap();

        // Wait for the reader thread to parse the output into the grid.
        for _ in 0..200 {
            let ready = window
                .update(cx, |v, _, _| {
                    let term = v.terminal.term.lock();
                    let grid = term.grid();
                    (0..grid.screen_lines() as i32)
                        .any(|l| (0..grid.columns()).any(|c| grid[Line(l)][Column(c)].c == 'W'))
                })
                .unwrap();
            if ready {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        window
            .update(cx, |view, window, cx| {
                fn set_query(
                    view: &mut TerminalView,
                    q: &str,
                    window: &mut Window,
                    cx: &mut Context<TerminalView>,
                ) {
                    let input = view.search.as_ref().unwrap().input.clone();
                    input.update(cx, |s, cx| s.set_value(q, window, cx));
                    view.recompute_matches(cx);
                }

                view.open_search(window, cx);
                assert!(view.search.is_some(), "Cmd+F opens the bar");

                // Smart-case default: a lowercase query matches all three casings.
                set_query(view, "world", window, cx);
                assert_eq!(view.search.as_ref().unwrap().matches.len(), 3);
                assert!(!view.search_regex_error);

                // Force case-sensitive: only the exact-lowercase line matches.
                view.search_case_sensitive = true;
                view.recompute_matches(cx);
                assert_eq!(view.search.as_ref().unwrap().matches.len(), 1);
                view.search_case_sensitive = false;

                // Literal mode: "wor.d" (a literal dot) matches nothing; regex
                // mode turns "." into a wildcard so all three lines match.
                set_query(view, "wor.d", window, cx);
                assert_eq!(view.search.as_ref().unwrap().matches.len(), 0);
                view.search_regex = true;
                view.recompute_matches(cx);
                assert_eq!(view.search.as_ref().unwrap().matches.len(), 3);

                // A broken regex flags an error rather than a silent zero-match.
                view.search_regex = true;
                set_query(view, "(", window, cx);
                assert!(view.search_regex_error);
                assert_eq!(view.search.as_ref().unwrap().matches.len(), 0);
                // The same query is a valid literal once regex mode is off.
                view.search_regex = false;
                view.recompute_matches(cx);
                assert!(!view.search_regex_error);

                // Closing remembers the query for the next open.
                view.close_search(window, cx);
                assert_eq!(view.search_last_query, "(");
                assert!(view.search.is_none());
            })
            .unwrap();
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

    /// Copy-on-select, end to end: real output through the pump, the same
    /// start/update/end calls the mouse handlers make, then the clipboard.
    /// Off (the default) the release must leave the clipboard alone; on, the
    /// selected text lands at mouse-up with no ⌘C.
    #[gpui::test]
    fn copy_on_select_writes_the_clipboard_at_mouse_up(cx: &mut TestAppContext) {
        let (window, mut daemon) = harness(cx);

        DaemonMsg::Output(b"hello world".to_vec())
            .encode(&mut daemon)
            .unwrap();
        // Bounded poll for the reader thread, as in the pump test above.
        for _ in 0..400 {
            cx.run_until_parked();
            let row: String = window
                .update(cx, |view, _, _| {
                    let term = view.terminal.term.clone();
                    let term = term.lock();
                    (0..11)
                        .map(|c| term.grid()[alacritty_terminal::index::Line(0)][Column(c)].c)
                        .collect()
                })
                .unwrap();
            if row == "hello world" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        // Drag across "hello" and release with the feature off: no copy.
        let drag_hello = |cx: &mut TestAppContext| {
            window
                .update(cx, |view, _, cx| {
                    view.on_select_start(0, 0, true, 1, false, cx);
                    view.on_select_update(4, 0, false, cx);
                    view.on_select_end(cx);
                })
                .unwrap();
        };
        drag_hello(cx);
        assert_eq!(
            cx.update(|cx| cx.read_from_clipboard()),
            None,
            "default-off must never write the clipboard"
        );

        // Same gesture with the feature on: "hello" is on the clipboard.
        cx.update(|cx| cx.update_global::<Config, _>(|cfg, _| cfg.copy_on_select = true));
        drag_hello(cx);
        let text = cx.update(|cx| cx.read_from_clipboard().and_then(|item| item.text()));
        assert_eq!(text.as_deref(), Some("hello"));

        // The mouse-up copy must NOT consume the selection: copy-on-select
        // keeps the highlight, like every terminal with the feature.
        let selected = window
            .update(cx, |view, _, _| {
                view.terminal.term.lock().selection.is_some()
            })
            .unwrap();
        assert!(
            selected,
            "copy-on-select must keep the selection highlighted"
        );
    }

    /// Ctrl+C means "copy the selection, else ^C (SIGINT)" — so the copy must
    /// consume the selection, or a second Ctrl+C copies again forever and the
    /// user can't interrupt the foreground command (#111). Cmd+C keeps the
    /// selection (macOS convention), covered by the copy-on-select test above.
    #[gpui::test]
    fn ctrl_c_copy_consumes_the_selection_so_the_next_press_is_sigint(cx: &mut TestAppContext) {
        let (window, mut daemon) = harness(cx);

        DaemonMsg::Output(b"hello world".to_vec())
            .encode(&mut daemon)
            .unwrap();
        // Bounded poll for the reader thread, as in the pump test above.
        for _ in 0..400 {
            cx.run_until_parked();
            let row: String = window
                .update(cx, |view, _, _| {
                    let term = view.terminal.term.clone();
                    let term = term.lock();
                    (0..11)
                        .map(|c| term.grid()[alacritty_terminal::index::Line(0)][Column(c)].c)
                        .collect()
                })
                .unwrap();
            if row == "hello world" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        window
            .update(cx, |view, window, cx| {
                // Mouse-select "hello" (copy-on-select is off by default, so
                // the selection survives mouse-up).
                view.on_select_start(0, 0, true, 1, false, cx);
                view.on_select_update(4, 0, false, cx);
                view.on_select_end(cx);
                assert!(view.has_selection(), "the drag must leave a selection");

                // First Ctrl+C: copies and consumes the selection.
                let consumed = view.handle_cmd_shortcut(&key("ctrl-c"), window, cx);
                assert!(matches!(consumed, CmdKey::Consumed));
                assert!(
                    !view.has_selection(),
                    "the Ctrl+C copy must consume the selection"
                );

                // Second Ctrl+C: no selection left, so the chord falls through
                // to the raw ^C (SIGINT) path.
                let fell_through = view.handle_cmd_shortcut(&key("ctrl-c"), window, cx);
                assert!(matches!(fell_through, CmdKey::FallThrough));
            })
            .unwrap();
        let text = cx.update(|cx| cx.read_from_clipboard().and_then(|item| item.text()));
        assert_eq!(text.as_deref(), Some("hello"));
    }

    /// Pasting to the PTY consumes the selection like typing does, so the
    /// reported select → copy → paste → Ctrl+C sequence ends in SIGINT (#111).
    #[gpui::test]
    fn paste_to_the_pty_consumes_the_selection(cx: &mut TestAppContext) {
        let (window, mut daemon) = harness(cx);
        window
            .update(cx, |view, _, cx| {
                view.select_all(cx);
                assert!(view.has_selection());
                view.paste("echo hi".into(), cx);
                assert!(
                    !view.has_selection(),
                    "a PTY paste must consume the selection"
                );
            })
            .unwrap();
        // The pasted bytes still reach the PTY.
        assert_eq!(next_input(&mut daemon), b"echo hi".to_vec());
    }

    /// Same dual-purpose rule at the prompt: Ctrl+A selects the edited line,
    /// Ctrl+C copies it — and must consume the editor selection so the next
    /// Ctrl+C reaches the editor's ^C (abort line) instead of copying again
    /// (#111, editor-selection variant).
    #[gpui::test]
    fn ctrl_c_copy_consumes_the_editor_selection_at_the_prompt(cx: &mut TestAppContext) {
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
        // Poll until the prompt report has applied.
        for _ in 0..400 {
            cx.run_until_parked();
            let active = window.update(cx, |view, _, _| view.input_active()).unwrap();
            if active {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        window
            .update(cx, |view, window, cx| {
                assert!(view.input_active(), "the inline editor must be active");
                view.cmd.insert_str("echo hi");
                view.cmd.select_all();

                // First Ctrl+C: copies the line and consumes the selection.
                let consumed = view.handle_cmd_shortcut(&key("ctrl-c"), window, cx);
                assert!(matches!(consumed, CmdKey::Consumed));
                assert!(
                    view.cmd.selection().is_none(),
                    "the Ctrl+C copy must consume the editor selection"
                );

                // Second Ctrl+C: nothing selected anywhere, so the chord falls
                // through to the editor's ^C (abort line) handling.
                let fell_through = view.handle_cmd_shortcut(&key("ctrl-c"), window, cx);
                assert!(matches!(fell_through, CmdKey::FallThrough));
            })
            .unwrap();
        let text = cx.update(|cx| cx.read_from_clipboard().and_then(|item| item.text()));
        assert_eq!(text.as_deref(), Some("echo hi"));
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

    /// A generator that finishes while its menu is still open merges its results
    /// in: candidates land, the set filters to the word as it now stands, and the
    /// highlight settles on the closest match — the async half of #51's fix.
    #[gpui::test]
    fn generator_results_merge_into_the_open_menu(cx: &mut TestAppContext) {
        use crate::terminal::generator::Parsed;

        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, _, cx| {
                // A pure-generator slot: the menu opened with no sync candidates
                // (word_start at the caret, empty open word). The user has since
                // typed "ma", so the live word narrows what the results show.
                view.cmd.set_with_cursor("git checkout ma", 15);
                let session = CompletionSession::new(13, String::new(), Vec::new());
                let generation = view.open_completion(session);

                let results = vec![
                    Parsed {
                        text: "main".into(),
                        description: Some("branch".into()),
                    },
                    Parsed {
                        text: "mainline".into(),
                        description: Some("branch".into()),
                    },
                    Parsed {
                        text: "feature".into(),
                        description: None,
                    },
                ];
                view.completion_merge(generation, results, cx);

                let s = view.completion.as_ref().expect("menu still open");
                let shown: Vec<&str> = s.filtered.iter().map(|&i| s.all[i].text.as_str()).collect();
                // "feature" filtered out by the live "ma"; closeness orders the
                // rest; the top row is preselected.
                assert_eq!(shown, vec!["main", "mainline"]);
                assert_eq!(s.selected().unwrap().text, "main");
            })
            .unwrap();
    }

    /// A generator that finishes *after* its menu closed must not resurrect it:
    /// closing bumps the generation, so the stale result is dropped.
    #[gpui::test]
    fn generator_result_for_a_closed_menu_is_dropped(cx: &mut TestAppContext) {
        use crate::terminal::generator::Parsed;

        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, _, cx| {
                view.cmd.set_with_cursor("git checkout ", 13);
                let session = CompletionSession::new(13, String::new(), Vec::new());
                let stale = view.open_completion(session);
                // The user dismisses the menu before the generator returns.
                view.close_completion();

                view.completion_merge(
                    stale,
                    vec![Parsed {
                        text: "main".into(),
                        description: None,
                    }],
                    cx,
                );
                assert!(
                    view.completion.is_none(),
                    "a result for a closed session never reopens the menu"
                );

                // And a result for an old generation can't bleed into a *new*
                // session that has since opened.
                let fresh =
                    view.open_completion(CompletionSession::new(13, String::new(), Vec::new()));
                assert_ne!(stale, fresh);
                view.completion_merge(
                    stale,
                    vec![Parsed {
                        text: "main".into(),
                        description: None,
                    }],
                    cx,
                );
                let s = view.completion.as_ref().unwrap();
                assert!(
                    s.all.is_empty(),
                    "the stale result stayed out of the new menu"
                );
            })
            .unwrap();
    }

    /// A remote-workspace pane's cwd is a path on the **far** machine, and the
    /// pane itself never says so: `tty7-server` over there spawned an ordinary
    /// local shell and reports `remote_context: None`, exactly as a local
    /// daemon would. The machine that moved is the *daemon*, which no
    /// pane-level signal can express — only this side's workspace binding
    /// knows.
    ///
    /// So both accessors have to consult it, and they fail in opposite
    /// directions when they don't:
    ///   - `local_cwd` says yes and hands `/home/me/proj` to `read_dir`, to the
    ///     link opener, and to Tab's local path engine — answers about *this*
    ///     machine dressed as the remote's;
    ///   - `remote_ssh_cwd` says no, so Tab never asks the remote over SFTP and
    ///     silently hands the line to the shell instead. That one is only
    ///     annoying; the first one is wrong.
    #[gpui::test]
    fn a_remote_workspace_pane_reports_its_cwd_as_remote(cx: &mut TestAppContext) {
        use std::io::Write as _;
        let (window, mut daemon) = harness(cx);
        DaemonMsg::Cwd(std::path::PathBuf::from("/home/me/proj"))
            .encode(&mut daemon)
            .unwrap();
        daemon.flush().unwrap();
        // The reader is a real thread; poll rather than sleep a fixed span.
        for _ in 0..200 {
            let seen = window
                .update(cx, |view, _, _| view.cwd().is_some())
                .unwrap();
            if seen {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        window
            .update(cx, |view, _, cx| {
                // Unbound, this is a plain local pane: the path is this
                // machine's, and there is no remote to ask about it.
                assert_eq!(
                    view.local_cwd(),
                    Some(std::path::PathBuf::from("/home/me/proj"))
                );
                assert_eq!(view.remote_ssh_cwd(), None);

                bind_to_a_disconnected_remote_workspace(view, cx);

                // The premise: nothing about the pane became remote.
                assert!(
                    view.remote_context().is_none(),
                    "the far daemon reports a plain local pane — if this ever \
                     stops holding, the binding below is no longer the only signal"
                );
                assert_eq!(
                    view.local_cwd(),
                    None,
                    "a routed pane's cwd is not a path on this machine"
                );
                assert_eq!(
                    view.remote_ssh_cwd(),
                    Some("/home/me/proj".to_string()),
                    "Tab must ask the workspace's connection about it"
                );
            })
            .unwrap();
    }
}

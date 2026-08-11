use alacritty_terminal::event::Event as AlacEvent;
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Direction, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::TermMode;
use gpui::{
    App, ClipboardEntry, ClipboardItem, Context, ExternalPaths, FocusHandle, Focusable, Font,
    KeyDownEvent, Modifiers, MouseButton, MouseDownEvent, Pixels, ScrollDelta, ScrollWheelEvent,
    WeakEntity, Window, actions, div, prelude::*, px,
};
use gpui_component::kbd::Kbd;
use gpui_component::menu::{ContextMenuExt, PopupMenuItem};
use gpui_component::scroll::Scrollbar;
use gpui_component::{ActiveTheme as _, Icon, IconName, WindowExt as _, h_flex};

use super::TermSize;
use super::cmd_editor::CmdEditor;
use super::completion::{self, CandidateKind, CompletionSession};
use super::element::TerminalElement;
use super::highlight::{self, TokenKind};
use super::hold::{GapHold, Verdict};
use super::remote::RemoteTerminal;
use super::reverse_search::{self, ReverseSearch};
use super::scrollbar::{GridScroll, TerminalScrollHandle};
use super::search::{LinkTarget, SearchState};
use super::typeahead::{RawInput, Typeahead};
use crate::core::actions::{
    CloseActiveTab, DecreaseFontSize, ForkAgentSessionDown, ForkAgentSessionLeft,
    ForkAgentSessionRight, ForkAgentSessionUp, IncreaseFontSize, NewTab, SendBackTab, SendTab,
    SplitDown, SplitRight, ToggleMaximizePane,
};
use crate::core::config::{BellMode, Config, NotifyMode};
use crate::daemon::protocol::{RemoteContext, ShellSpec};
use crate::ui::i18n::{L10nKey, t, t_fmt};

const GRID_PAD_X: f32 = 8.;
const GRID_PAD_Y: f32 = 4.;

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
        InsertNewline,
        InsertNewlineFallback
    ]
);

pub struct ChildExited;

impl gpui::EventEmitter<ChildExited> for TerminalView {}

pub struct AuthPromptReady;

impl gpui::EventEmitter<AuthPromptReady> for TerminalView {}

pub struct AgentSessionChanged;

impl gpui::EventEmitter<AgentSessionChanged> for TerminalView {}

pub struct NativeSshParts {
    terminal: RemoteTerminal,
    pane_id: u64,
    persist: Box<crate::daemon::protocol::NativeSshSpec>,
}

pub struct ShellParts {
    terminal: RemoteTerminal,
    pub(crate) pane_id: u64,
    shell_spec: Option<ShellSpec>,
    pub(crate) workspace: Option<crate::terminal::PaneWorkspace>,
    pub(crate) restored: bool,
    pub(crate) owner: Option<crate::core::session::WorkspaceId>,
}

#[derive(Clone, Copy)]
struct DragScroll {
    overshoot: f32,
    col: usize,
    side: Side,
}

/// In-flight wheel animation. `remaining` is what is left to scroll, in lines,
/// relative to wherever the view happens to be — deliberately not an absolute
/// target, so output arriving mid-animation shifts the grid under us without
/// dragging the animation somewhere else.
#[derive(Clone, Copy)]
struct ScrollAnim {
    remaining: f32,
    last: std::time::Instant,
}

/// Fraction of the remaining distance consumed per [`SCROLL_ANIM_FRAME`].
const SCROLL_ANIM_SMOOTH: f32 = 0.4;
/// The frame `SCROLL_ANIM_SMOOTH` is calibrated against: one nominal 60 Hz
/// frame. Decay is scaled by real elapsed time, so the same feel holds at any
/// refresh rate.
const SCROLL_ANIM_FRAME: std::time::Duration = std::time::Duration::from_millis(16);
/// Below this much left to travel, land instead of asymptoting toward it. A
/// twentieth of a line is around a pixel — the tail of an exponential decay is
/// invisible long before it ends, and every frame of it costs a full repaint.
const SCROLL_ANIM_MIN: f32 = 0.05;
/// A jump smaller than this reads as continuous already; spreading it would
/// only put lag between the hand and the grid. Inching a wheel one detent at a
/// time lands here, and so does every event a trackpad sends.
const SCROLL_ANIM_MIN_JUMP: f32 = 1.0;
/// How long a trackpad gesture stays "live" after its last event. Long enough
/// to bridge the gaps in a momentum tail, short enough that reaching for the
/// wheel right after a swipe is not mistaken for more of the swipe.
const SCROLL_GESTURE_IDLE: std::time::Duration = std::time::Duration::from_millis(150);
/// How far a trackpad has to travel, in lines, to earn one font-size step while
/// the platform modifier is held. A wheel detent is one step on its own, so
/// this only ever applies to the continuous stream a gesture produces.
const ZOOM_SCROLL_LINES: f32 = 3.0;

fn cwd_is_on_host(pane_runs_remotely: bool, host_is_local: bool) -> bool {
    match pane_runs_remotely {
        false => host_is_local,
        true => !host_is_local,
    }
}

pub struct TerminalView {
    pub terminal: RemoteTerminal,
    host_id: crate::ui::host_ops::HostId,
    workspace: Option<crate::terminal::PaneWorkspace>,
    pub pane_id: u64,
    shell_spec: Option<ShellSpec>,
    owner_workspace: Option<crate::core::session::WorkspaceId>,
    restored: bool,
    ssh_spec: Option<Box<crate::daemon::protocol::NativeSshSpec>>,
    /// The verified remote staging directory for pasted images, once one has
    /// been prepared for this pane. `None` means "not prepared yet", never
    /// "preparation failed" — see [`staging_cache`].
    remote_clipboard_dir: Option<String>,
    pub focus_handle: FocusHandle,
    pub font: Font,
    pub font_bold: Option<Font>,
    pub font_italic: Option<Font>,
    font_features: Option<gpui::FontFeatures>,
    pub font_size: Pixels,
    pub line_height_mul: f32,
    pub cell_width: Pixels,
    pub(super) line_height: Pixels,
    selecting: bool,
    drag_scroll: Option<DragScroll>,
    drag_scroll_epoch: u64,
    scroll_anim: Option<ScrollAnim>,
    scroll_anim_epoch: u64,
    gesture_until: Option<std::time::Instant>,
    pub title: String,
    pub marked_text: String,
    last_mouse_cell: Option<(usize, usize)>,
    last_hover_cell: Option<(usize, usize)>,
    link_modifier_down: bool,
    /// The verdict of `should_show_context_menu` for the most recent right
    /// mouse-down, latched so the menu builder — which gpui-component runs on a
    /// deferred callback, one turn after the click — can still see the
    /// modifiers the user actually held.
    context_menu_allowed: bool,
    scroll_debt: f32,
    /// Lines travelled under the zoom modifier that have not yet added up to a
    /// font-size step. Kept apart from [`scroll_debt`](Self::scroll_debt) so
    /// letting go of the modifier mid-gesture cannot hand the leftovers of one
    /// to the other.
    zoom_debt: f32,
    pub(super) scroll_frac: f32,
    /// The scrollback bar's end of the grid: where it thinks the viewport is,
    /// and where it has asked for it to go. See [`super::scrollbar`].
    pub(super) scroll_handle: TerminalScrollHandle,
    pub search: Option<SearchState>,
    pub cursor_visible: bool,
    pub focused: bool,
    pub(super) search_focused: bool,
    pub(super) search_case_sensitive: bool,
    pub(super) search_regex: bool,
    pub(super) search_regex_error: bool,
    pub(super) search_last_query: String,
    /// Bumped by every wakeup that reaches an open search bar, so the pending
    /// rescan can tell "the pane went quiet" from "more output landed".
    pub(super) search_scan_epoch: u64,
    /// Whether a rescan is already waiting out the debounce. One task at a
    /// time, however fast the pane is printing.
    pub(super) search_scan_armed: bool,
    pub bell_flash: bool,
    pub report_mouse: bool,
    last_at_prompt: bool,
    last_typeahead_blocked: bool,
    running_since: Option<std::time::Instant>,
    running_title: String,
    running_agent: Option<crate::core::cli_agent::CLIAgent>,
    last_agent_status: Option<crate::core::cli_agent::AgentStatus>,
    last_agent_session: (Option<String>, Option<Vec<String>>),
    agent_turn_started: Option<std::time::Instant>,
    agent_was_rich: bool,
    agent_result_unread: bool,
    keep_unread_on_focus: bool,
    git_status_cwd: Option<std::path::PathBuf>,
    last_agent_activity: u64,
    cmd: CmdEditor,
    typeahead: Typeahead,
    hold: GapHold,
    history: Vec<String>,
    history_counts: std::collections::HashMap<String, u32>,
    history_cwds: std::collections::HashMap<String, std::collections::HashSet<String>>,
    history_meta: std::collections::HashMap<String, super::history::EntryMeta>,
    history_ranked: Vec<String>,
    history_frecency: Vec<f64>,
    history_scope: super::history::Scope,
    ranked_cwd: Option<std::path::PathBuf>,
    history_nav: Option<usize>,
    history_stash: String,
    last_word_nav: Option<LastWordWalk>,
    pending_history: Option<PendingHistory>,
    completion: Option<CompletionSession>,
    remote_completion_inflight: bool,
    completion_generation: u64,
    editor_handoff: Option<u64>,
    editor_handoff_interrupt_seq: Option<u64>,
    reverse_search: Option<ReverseSearch>,
    integration_notice: Option<String>,
    integration_notice_shown: bool,
    created_at: std::time::Instant,
    editor_selecting: bool,
    editor_select_gesture: bool,
    editor_drag_word: Option<(usize, usize)>,
    editor_goal_col: Option<usize>,
    pub(super) hovered_link: Option<HoveredLink>,
    /// How opaque the pane wants this terminal painted this frame: 1.0 at
    /// rest, [`crate::ui::pane::INACTIVE_DIM`] for an unfocused pane in a
    /// split, [`crate::ui::pane::LIFTED_DIM`] while the pane is being
    /// dragged. The pane leaf computes it and stores it here, because the
    /// dim is applied by blending the terminal's own colours toward the
    /// window background instead of the pane's element opacity — an opacity
    /// style would alpha-multiply every quad and path separately, so a
    /// powerline triangle stacked over a segment quad would show the
    /// already-dimmed segment through its own (1-dim) alpha and land with a
    /// visible seam where it meets the segment. The terminal element resets
    /// the field to 1.0 at the end of every paint, so the value never
    /// outlives the frame it was written for.
    pub(super) dim: f32,
    _focus_subs: Vec<gpui::Subscription>,
}

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

#[derive(Clone, Debug, PartialEq)]
pub(super) enum LoopbackPlan {
    Direct,
    NoForwardNeeded,
    ForwardOnPane(u64),
    ForwardOnWorkspace(Box<crate::terminal::PaneWorkspace>),
}

pub(super) fn loopback_plan(
    enabled: bool,
    workspace: Option<&crate::terminal::PaneWorkspace>,
    remote_kind: Option<crate::daemon::protocol::RemoteKind>,
    pane_id: u64,
) -> LoopbackPlan {
    if !enabled {
        return LoopbackPlan::Direct;
    }
    if let Some(ws) = workspace {
        if ws.shares_localhost() {
            return LoopbackPlan::NoForwardNeeded;
        }
        if ws.spec.is_none() {
            log::warn!("remote workspace has no connection spec; not forwarding localhost links");
            return LoopbackPlan::Direct;
        }
        return LoopbackPlan::ForwardOnWorkspace(Box::new(ws.clone()));
    }
    match remote_kind {
        Some(crate::daemon::protocol::RemoteKind::NativeSsh) => {
            LoopbackPlan::ForwardOnPane(pane_id)
        }
        _ => LoopbackPlan::Direct,
    }
}

struct PendingHistory {
    line: String,
    cwd: Option<std::path::PathBuf>,
    ts: u64,
    seq: u64,
}

struct LastWordWalk {
    entry: usize,
    at: usize,
    word: String,
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

enum CmdKey {
    Consumed,
    Bubble,
    FallThrough,
}

const HOLD_WINDOW: std::time::Duration = std::time::Duration::from_millis(150);

const INTEGRATION_GRACE: std::time::Duration = std::time::Duration::from_secs(8);

const INTEGRATION_NOTICE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

const OPPORTUNISTIC_GIT_GAP: std::time::Duration = std::time::Duration::from_millis(1500);

const MAX_HISTORY_BYTES: u64 = 4 << 20;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum GitRefresh {
    Edge,
    Opportunistic,
}

fn known_pty_shim(fg: &str) -> Option<&'static str> {
    ["kiro-cli-term", "figterm", "qterm", "cwterm"]
        .into_iter()
        .find(|shim| fg.contains(shim))
}

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

/// Join whatever context a pane has into a notification title, most specific
/// part first: an agent name if one is running, otherwise the machine the pane
/// lives on, then the workspace it belongs to. Kept to two segments — Windows
/// toast titles are a single line and ellipsize anything longer.
fn compose_notification_title(
    lead: Option<String>,
    host: Option<String>,
    workspace: Option<String>,
) -> String {
    match (lead.or(host), workspace) {
        (Some(lead), Some(workspace)) => format!("{lead} · {workspace}"),
        (Some(only), None) | (None, Some(only)) => only,
        (None, None) => "tty7".to_string(),
    }
}

/// The longest command line to put in a confirmation. The shell sends up to
/// 512 bytes; a dialog asking whether to end your work should still read as a
/// sentence.
const BUSY_COMMAND_MAX: usize = 60;

/// Undo the escaping the shell integration applies to an OSC 133;C payload so
/// it cannot break OSC framing: `%` and the four control bytes. Anything else
/// is left exactly as the user typed it.
fn unescape_mark_text(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(i) = rest.find('%') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let (decoded, width) = match tail.get(..3) {
            Some("%25") => ("%", 3),
            Some("%1B") | Some("%07") | Some("%0D") => ("", 3),
            Some("%0A") => (" ", 3),
            _ => ("%", 1),
        };
        out.push_str(decoded);
        rest = &tail[width..];
    }
    out.push_str(rest);
    out
}

fn clamp_command(cmd: &str) -> String {
    let cmd = cmd.trim();
    match cmd.chars().count() > BUSY_COMMAND_MAX {
        false => cmd.to_string(),
        true => format!(
            "{}…",
            cmd.chars()
                .take(BUSY_COMMAND_MAX)
                .collect::<String>()
                .trim_end()
        ),
    }
}

impl TerminalView {
    fn notify_command_finished(
        &self,
        label: &str,
        elapsed: std::time::Duration,
        cx: &mut Context<Self>,
    ) {
        let secs = elapsed.as_secs().to_string();
        let command = label.trim();
        let body = if command.is_empty() {
            t_fmt(L10nKey::NotifyCommandFinished, &[("secs", &secs)])
        } else {
            t_fmt(
                L10nKey::NotifyCommandFinishedWithCommand,
                &[("command", command), ("secs", &secs)],
            )
        };
        self.notify_pane(None, &body, cx);
    }

    fn notify_agent_finished(
        &self,
        agent: crate::core::cli_agent::CLIAgent,
        elapsed: std::time::Duration,
        cx: &mut Context<Self>,
    ) {
        let secs = elapsed.as_secs().to_string();
        let body = t_fmt(L10nKey::NotifyAgentFinished, &[("secs", &secs)]);
        self.notify_pane(Some(agent.display_name()), &body, cx);
    }

    /// Notify about this pane, with a title describing where it lives and a
    /// click that reveals it. The id has to be the pane's *gpui entity id*:
    /// that is what `TrayAction::RevealPane` matches leaves on, and it is a
    /// different number from `pane_id`, which the daemon assigns.
    fn notify_pane(&self, lead: Option<&str>, body: &str, cx: &mut Context<Self>) {
        let title = self.notification_title(lead, cx);
        super::remote::notify_desktop_for_pane(Some(&title), body, Some(cx.entity_id()));
    }

    fn notification_title(&self, lead: Option<&str>, cx: &App) -> String {
        let host = self
            .workspace
            .as_ref()
            .map(|w| crate::ui::remote_connect::target_label(cx, &w.target));
        let workspace = self
            .owner_workspace
            .and_then(|id| crate::ui::machine_mirror::display_name_for(cx, id));
        compose_notification_title(lead.map(str::to_string), host, workspace)
    }
}

/// Ring the platform's alert sound, reporting whether one was actually made —
/// `Audible` falls back to a flash when it wasn't, so a silent `false` is the
/// difference between "you heard the bell" and "you saw it instead".
///
/// Linux has no equivalent worth the dependency: the desktop sound APIs
/// (libcanberra, PipeWire) are a runtime link away and X11's `XBell` does
/// nothing under Wayland, so the flash fallback stays the answer there.
fn ring_system_bell() -> bool {
    #[cfg(target_os = "macos")]
    {
        objc2_app_kit::NSBeep();
        true
    }
    #[cfg(target_os = "windows")]
    {
        // MB_OK is the "Default Beep" scheme entry, so this follows whatever
        // the user picked in Sound Settings — including "None", which is a
        // deliberate silence and still reports success. Deliberately not
        // `Beep()`, which synthesizes a fixed tone straight at the speaker and
        // ignores the scheme. Returns immediately; the sound plays async.
        use windows_sys::Win32::System::Diagnostics::Debug::MessageBeep;
        use windows_sys::Win32::UI::WindowsAndMessaging::MB_OK;
        unsafe { MessageBeep(MB_OK) != 0 }
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        false
    }
}

fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed {
        let text = text.replace("\r\n", "\n");
        let mut bytes = b"\x1b[200~".to_vec();
        bytes.extend(text.bytes().filter(|&b| b != 0x1b));
        bytes.extend_from_slice(b"\x1b[201~");
        bytes
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
    }
}

fn submit_bytes(line: &str, bracketed: bool) -> Vec<u8> {
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

fn trim_trailing_spaces(text: &str) -> String {
    text.split('\n')
        .map(|line| line.trim_end_matches([' ', '\t']))
        .collect::<Vec<_>>()
        .join("\n")
}

fn shell_escape_path(path: &str) -> String {
    if path.is_empty() {
        return "''".to_string();
    }
    if path.contains(['\n', '\r']) {
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

fn escape_candidate(text: &str) -> String {
    match text.strip_prefix("~/") {
        Some(rest) => format!("~/{}", shell_escape_path(rest)),
        None => shell_escape_path(text),
    }
}

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

/// The connection a paste should be uploaded over, when this pane runs on a
/// remote host reachable over the daemon's russh stack: either a native SSH
/// workspace pane or a standalone native SSH pane. WSL shares localhost and
/// needs path translation instead, and a workspace without connection details
/// has no channel to piggyback on — both keep the local-path behavior.
fn remote_paste_spec<'a>(
    workspace: Option<&'a crate::terminal::PaneWorkspace>,
    ssh_spec: Option<&'a crate::daemon::protocol::NativeSshSpec>,
) -> Option<&'a crate::daemon::protocol::NativeSshSpec> {
    if let Some(ws) = workspace {
        if ws.shares_localhost() {
            return None;
        }
        return ws.spec.as_deref();
    }
    ssh_spec
}

/// Whether a pane stages the clipboard image to a file instead of forwarding
/// SYN and letting the agent read the clipboard itself.
///
/// SYN only works because the agent shares a clipboard with the pane. That
/// holds for a local macOS pane — where it is the better path, carrying the
/// image at full fidelity — and nowhere else: off macOS Claude Code silently
/// drops raw screenshots (anthropics/claude-code#26679), and an agent in a
/// remote pane reads the clipboard of the host it runs on, which never holds
/// this machine's screenshot no matter what the local OS is.
fn stages_clipboard_image(is_remote: bool) -> bool {
    cfg!(not(target_os = "macos")) || is_remote
}

/// The WSL view of a Windows path: `C:\x\y` becomes `/mnt/c/x/y`.
///
/// `None` for anything without a drive letter — a UNC temp directory has no
/// automount mapping, and `C:x` is drive-relative rather than absolute.
///
/// The `/mnt` prefix is WSL's default automount root, not a guaranteed one:
/// `[automount] root=` in `/etc/wsl.conf` can move it. Asking the distro
/// (`wslpath -u`) would be exact, but it is a round trip through the daemon on
/// a keystroke path, and a moved automount root is rare enough that a wrong
/// path — which the user sees, in their own line, before they send it — beats
/// making every paste wait on a subprocess.
fn wsl_path(windows: &str) -> Option<String> {
    let mut chars = windows.chars();
    let drive = chars.next()?.to_ascii_lowercase();
    if !drive.is_ascii_alphabetic() || chars.next()? != ':' {
        return None;
    }
    if !matches!(chars.next()?, '\\' | '/') {
        return None;
    }
    Some(format!(
        "/mnt/{drive}/{}",
        chars.as_str().replace('\\', "/")
    ))
}

/// The Windows spelling of a WSL pane's POSIX cwd — [`wsl_path`]'s inverse,
/// for reading rather than writing: the distro's `\\wsl$` share is how a
/// local `read_dir` can list a directory this process cannot reach natively.
///
/// Everything stays on the share, `/mnt/<drive>` included. Mapping the
/// automount back to the drive letter would list faster, but an absolute
/// word completes against its *cwd's* path prefix (`resolve_dir` keeps the
/// prefix when a rooted word lands on it) — so a drive-spelled cwd would send
/// `ls /etc<Tab>` to `C:\etc` instead of the distro's `/etc`. One prefix,
/// one meaning. A distro name with a path separator cannot name a share.
fn wsl_share_path(distro: &str, posix: &str) -> Option<std::path::PathBuf> {
    if distro.is_empty() || distro.contains(['\\', '/']) {
        return None;
    }
    let rest = posix.strip_prefix('/')?;
    Some(std::path::PathBuf::from(format!(
        r"\\wsl$\{distro}\{}",
        rest.replace('/', "\\")
    )))
}

/// The distro whose `\\wsl$` share holds a pane's filesystem, or `None` when
/// the pane is not a WSL one and Tab belongs to the shell.
///
/// The two kinds of WSL pane are told apart by who reports the distro. A pane
/// that runs `wsl.exe` is tagged by its remote context, and it reaches the
/// distro of whichever machine *hosts* it — so only this machine's panes may
/// take the share; a remote host's same-named distro would list the wrong
/// files. A pane in a WSL workspace is tagged by its workspace target instead,
/// and needs no such check: tty7 reaches those distros by running `wsl.exe`
/// here, so the share is this machine's by construction — even though the
/// pane's host, being the distro's own server, is not `HostId::LOCAL`.
fn wsl_share_distro(
    remote: Option<&crate::daemon::protocol::RemoteContext>,
    workspace: Option<&crate::terminal::PaneWorkspace>,
    host_is_local: bool,
) -> Option<String> {
    match remote {
        Some(remote) => (remote.kind == crate::daemon::protocol::RemoteKind::Wsl && host_is_local)
            .then(|| remote.target.clone()),
        None => match &workspace?.target {
            crate::core::session::RemoteTarget::Wsl { distro } => Some(distro.clone()),
            _ => None,
        },
    }
}

/// The staged image's path as the pane's own filesystem spells it.
///
/// A WSL pane shares this machine's disk but not its path syntax: an agent in
/// there reads `/mnt/c/…` and cannot open `C:\…` at all, which is why the
/// upload route skips WSL — there is nothing to copy, only a name to rewrite.
/// A path with no mapping falls back to the Windows one, which at least tells
/// the user where the file is.
fn staged_path_for_pane(local: &str, shares_localhost: bool) -> String {
    if shares_localhost {
        return wsl_path(local).unwrap_or_else(|| local.to_string());
    }
    local.to_string()
}

/// Staging images under the SSH user's own home keeps them out of the
/// world-writable `/tmp`, where any local account could pre-create the
/// directory, read what lands in it, or swap a pasted screenshot for one of
/// its own before the pane's agent opens it.
const REMOTE_CLIPBOARD_PATH: [&str; 3] = [".cache", "tty7", "clipboard"];

/// Owner-only, and *only* owner: a staging directory anyone else can enter is
/// one anyone else can read the pasted screenshots out of.
const REMOTE_CLIPBOARD_MODE: u32 = 0o700;

/// Whether a prepared staging directory may be uploaded into.
///
/// The mode is what a `stat` reported *after* a `chmod 0700` the daemon
/// watched succeed, which is the ownership proof: POSIX only lets a file's
/// owner change its mode, so a directory tty7 can chmod and then observe at
/// exactly `0700` is one the SSH user owns and nobody else can enter. A
/// symlink is refused outright because `stat` follows links, so a link planted
/// at the staging path would otherwise be judged by its target.
fn staging_dir_is_safe(
    is_symlink: bool,
    kind: Option<crate::daemon::protocol::SftpEntryKind>,
    mode: u32,
) -> bool {
    use crate::daemon::protocol::SftpEntryKind;
    !is_symlink
        && matches!(kind, Some(SftpEntryKind::Dir))
        && mode & 0o7777 == REMOTE_CLIPBOARD_MODE
}

/// The staging directory to reuse on the next paste. Only a verified directory
/// is cached: a preparation that failed — a dropped link, a squatted path, a
/// remote with no POSIX `/home` — must be retried rather than latched, or
/// every later paste emits a remote path for a directory that was never
/// created.
fn staging_cache(prepared: &Result<String, String>) -> Option<String> {
    prepared.as_ref().ok().cloned()
}

/// Create and verify the per-user staging directory, answering the absolute
/// remote path to upload into. Blocking: every step is a daemon round trip
/// over the pane's SSH connection, so this only ever runs off the UI thread.
fn prepare_remote_clipboard_dir(route: &crate::ui::sftp::SftpRoute) -> Result<String, String> {
    use crate::daemon::protocol::{SftpOp, SftpOpResult};
    let home = match route.op(SftpOp::Realpath {
        path: ".".to_string(),
    }) {
        SftpOpResult::Link(home) if home.starts_with('/') => home,
        SftpOpResult::Error(e) => return Err(e),
        other => {
            return Err(format!(
                "the remote home directory is not a path: {other:?}"
            ));
        }
    };
    let mut dir = home;
    for component in REMOTE_CLIPBOARD_PATH {
        dir = crate::daemon::ssh::sftp::remote_join(&dir, component);
        // An existing directory fails here with EEXIST; the checks below are
        // what decide whether this one is ours, so the result carries no
        // information worth branching on.
        let _ = route.op(SftpOp::Mkdir { path: dir.clone() });
    }
    if let SftpOpResult::Link(target) = route.op(SftpOp::Readlink { path: dir.clone() }) {
        return Err(format!("{dir} is a symlink to {target}"));
    }
    if let SftpOpResult::Error(e) = route.op(SftpOp::Chmod {
        path: dir.clone(),
        mode: REMOTE_CLIPBOARD_MODE,
    }) {
        return Err(format!("{dir} is not owned by this session: {e}"));
    }
    match route.op(SftpOp::Stat { path: dir.clone() }) {
        SftpOpResult::Stat(entry)
            if staging_dir_is_safe(false, Some(entry.kind), entry.permissions) =>
        {
            Ok(dir)
        }
        SftpOpResult::Stat(entry) => Err(format!(
            "{dir} is not a private directory (mode {:o})",
            entry.permissions & 0o7777
        )),
        SftpOpResult::Error(e) => Err(e),
        other => Err(format!("unexpected reply for {dir}: {other:?}")),
    }
}

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
                // The pane this one stands in for is gone, but its screen may
                // not be: if the daemon kept a copy, the new pane opens showing
                // it, under a line saying the shell below is new. Asking costs
                // nothing when there is no copy — the daemon answers by
                // spawning the blank pane it would have spawned anyway.
                let restore = restore_pane.map(|pane_id| crate::daemon::protocol::RestoreFrom {
                    pane_id,
                    banner: Some(
                        crate::ui::i18n::t(crate::ui::i18n::L10nKey::PaneRestoredScreenBanner)
                            .to_string(),
                    ),
                });
                let (terminal, id) = RemoteTerminal::spawn_on(
                    &route,
                    TermSize::new(80, 24),
                    8,
                    17,
                    working_directory,
                    shell.clone(),
                    owner.map(|id| id.to_string()),
                    restore,
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

    pub(crate) fn restored(&self) -> bool {
        self.restored
    }

    pub fn owner_workspace(&self) -> Option<crate::core::session::WorkspaceId> {
        self.owner_workspace
    }

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

    pub fn from_native_ssh_parts(
        parts: NativeSshParts,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self::with_terminal(parts.terminal, parts.pane_id, window, cx);
        view.ssh_spec = Some(parts.persist);
        view
    }

    fn with_terminal(
        terminal: RemoteTerminal,
        pane_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
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
                if woke {
                    let _ = this.update_in(cx, |_, window, _| window.refresh());
                }
            }
        })
        .detach();

        let focus_subs = vec![
            cx.on_focus_in(&focus_handle, window, |view, _window, cx| {
                view.focused = true;
                view.cursor_visible = true;
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

        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(530))
                    .await;
                if this
                    .update(cx, |view, cx| {
                        if view.focused {
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

        cx.on_release_in(window, |view, window, cx| {
            view.terminal.detach_link();
            for image in view.terminal.images().take_for_release() {
                cx.drop_image(image, Some(window));
            }
        })
        .detach();

        window.focus(&focus_handle, cx);

        let history = super::history::load(&super::history::Scope::Local);
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
            remote_clipboard_dir: None,
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
            scroll_anim: None,
            scroll_anim_epoch: 0,
            gesture_until: None,
            title: "tty7".to_string(),
            marked_text: String::new(),
            last_mouse_cell: None,
            report_mouse,
            last_hover_cell: None,
            link_modifier_down: false,
            context_menu_allowed: true,
            scroll_debt: 0.,
            zoom_debt: 0.,
            scroll_frac: 0.,
            scroll_handle: TerminalScrollHandle::default(),
            search: None,
            cursor_visible: true,
            focused: true,
            dim: 1.,
            search_focused: false,
            search_case_sensitive: false,
            search_regex: false,
            search_regex_error: false,
            search_last_query: String::new(),
            search_scan_epoch: 0,
            search_scan_armed: false,
            bell_flash: false,
            last_at_prompt: false,
            last_typeahead_blocked: false,
            running_since: None,
            running_title: String::new(),
            running_agent: None,
            last_agent_status: None,
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
            history_scope: super::history::Scope::Local,
            ranked_cwd: None,
            history_nav: None,
            history_stash: String::new(),
            last_word_nav: None,
            pending_history: None,
            completion: None,
            completion_generation: 0,
            editor_handoff: None,
            editor_handoff_interrupt_seq: None,
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

    pub fn set_grid_size(
        &mut self,
        cols: usize,
        rows: usize,
        cell_width: Pixels,
        line_height: Pixels,
        scale: f32,
    ) {
        if (cols, rows) != (self.terminal.size().cols, self.terminal.size().rows) {
            self.last_hover_cell = None;
            self.hovered_link = None;
        }
        self.cell_width = cell_width;
        self.line_height = line_height;
        // Report the cell size to the child in *device* pixels (logical × display
        // scale), so `ws_xpixel`/`ws_ypixel` describe the real framebuffer. A
        // pixel-aware program like terminal-browser renders its frame at that
        // native resolution; painted back into logical-pixel bounds, gpui blits
        // it ~1:1 on the framebuffer instead of upscaling a half-resolution
        // bitmap (which looked soft and magnified on Retina). This is what
        // kitty/ghostty report. `self.cell_width` stays logical — glyph layout
        // and mouse mapping work in logical pixels.
        let scale = if scale.is_finite() && scale > 0. {
            scale
        } else {
            1.
        };
        self.terminal.resize(
            TermSize::new(cols, rows),
            (cell_width.as_f32() * scale).round().max(1.) as u16,
            (line_height.as_f32() * scale).round().max(1.) as u16,
        );
    }

    pub fn cwd(&self) -> Option<std::path::PathBuf> {
        self.terminal.foreground_cwd()
    }

    /// Sets how opaque the pane wants this terminal painted; the pane leaf
    /// calls this every frame while rendering, and the terminal element
    /// blends its colours toward the window background during paint (see
    /// [`Self::dim`] for why that beats an element-opacity style). The
    /// element resets the value to 1.0 at the end of every paint, so a
    /// render site that forgets to set it gets full brightness next frame —
    /// never a stale dim.
    pub(crate) fn set_dim(&mut self, dim: f32) {
        self.dim = dim;
    }

    pub fn remote_context(&self) -> Option<RemoteContext> {
        self.terminal.remote_context()
    }

    pub fn local_cwd(&self) -> Option<std::path::PathBuf> {
        self.paths_are_local().then(|| self.cwd())?
    }

    fn paths_are_local(&self) -> bool {
        self.remote_context().is_none() && self.host_id.is_local()
    }

    pub fn spawnable_cwd(&self) -> Option<std::path::PathBuf> {
        self.remote_context().is_none().then(|| self.cwd())?
    }

    pub fn host(&self, cx: &gpui::App) -> Option<crate::ui::host_ops::SharedHost> {
        crate::ui::host_registry::HostRegistry::lookup(cx, self.host_id)
    }

    pub fn host_id(&self) -> crate::ui::host_ops::HostId {
        self.host_id
    }

    pub fn workspace(&self) -> Option<&crate::terminal::PaneWorkspace> {
        self.workspace.as_ref()
    }

    pub fn set_workspace(&mut self, workspace: Option<crate::terminal::PaneWorkspace>) {
        self.host_id = workspace
            .as_ref()
            .map_or(crate::ui::host_ops::HostId::LOCAL, |w| w.target.host_id());
        self.workspace = workspace;
    }

    pub fn pane_route(&self) -> crate::terminal::PaneRoute {
        crate::terminal::PaneRoute::for_workspace(self.workspace.as_ref())
    }

    fn accepts_input(&self, cx: &gpui::App) -> bool {
        let Some(ws) = self.workspace().map(|w| w.workspace) else {
            return true;
        };
        crate::ui::remote_workspace::workspace_accepts_input(cx, ws)
    }

    pub fn relink_plan(&self) -> (u64, TermSize, u16, u16) {
        (
            self.pane_id,
            self.terminal.size(),
            self.cell_width.as_f32().round() as u16,
            self.line_height.as_f32().round() as u16,
        )
    }

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

    pub fn detach_link(&mut self, cx: &mut Context<Self>) {
        self.terminal.detach_link();
        cx.notify();
    }

    pub fn host_cwd(&self) -> Option<std::path::PathBuf> {
        self.cwd_is_on_host().then(|| self.cwd())?
    }

    fn cwd_is_on_host(&self) -> bool {
        cwd_is_on_host(!self.paths_are_local(), self.host_id.is_local())
    }

    pub fn agent(&self) -> Option<crate::core::cli_agent::CLIAgent> {
        self.terminal.foreground_agent()
    }

    pub fn agent_session(&self) -> Option<crate::core::cli_agent::AgentSessionState> {
        self.terminal.agent_session()
    }

    /// What this pane is in the middle of, when it can say so. `None` means
    /// either nothing is running or the shell never told us — and a terminal
    /// that guessed would raise this question on every single close.
    pub fn busy(&self) -> Option<PaneBusy> {
        use crate::core::cli_agent::AgentStatus;
        // `Done` is the opposite of busy: the turn is over, and the badge
        // saying so is exactly what sends a reader to close the tab. Only a
        // turn still in flight — running, or stopped on a question — is work
        // that closing would cut short.
        if let Some(agent) = self.agent()
            && self
                .agent_session()
                .is_some_and(|s| matches!(s.status, AgentStatus::Working | AgentStatus::Waiting))
        {
            return Some(PaneBusy::Agent(agent.display_name()));
        }
        // Without shell integration `at_prompt` is permanently false, so
        // `running_since` is permanently Some. Only trust it when the shell is
        // actually reporting.
        if !self.terminal.shell_active() {
            return None;
        }
        self.running_since?;
        // Prefer what the shell said it was running. `running_title` is only
        // the window title as it stood when the command began, and a prompt
        // that titles by directory — a very common setup — made this ask
        // "tty7 is still running. Closing ends it." about a folder. The
        // OSC 133;C mark carries the submitted line itself.
        let named = Some(clamp_command(&unescape_mark_text(
            &self.terminal.running_command(),
        )))
        .filter(|t| !t.is_empty());
        Some(PaneBusy::Command(match named {
            Some(cmd) => cmd,
            None => match self.running_title.trim().is_empty() {
                true => self.title.clone(),
                false => self.running_title.clone(),
            },
        }))
    }

    pub fn agent_result_unread(&self) -> bool {
        self.agent_result_unread
    }

    pub fn mark_agent_result_unread(&mut self, refocus_incoming: bool) {
        self.agent_result_unread = true;
        self.keep_unread_on_focus = refocus_incoming;
    }

    pub fn git_status(&self, cx: &App) -> Option<crate::terminal::git_status::GitStatus> {
        let cwd = self.git_status_cwd.as_ref()?;
        cx.try_global::<crate::terminal::git_status::GitStatusCache>()?
            .status_for(self.host_id, cwd)
    }

    pub fn git_status_cwd(&self) -> Option<&std::path::Path> {
        self.git_status_cwd.as_deref()
    }

    pub fn refresh_git_status_now(&mut self, cx: &mut Context<Self>) {
        let cwd = self.git_status_cwd.clone();
        if cwd.is_some() {
            self.refresh_git_status(cwd, GitRefresh::Opportunistic, cx);
        }
    }

    pub fn selection_text(&self) -> Option<String> {
        self.terminal
            .term
            .lock()
            .selection_to_string()
            .filter(|t| !t.trim().is_empty())
    }

    pub fn send_agent_prompt(&self, prompt: &str) {
        self.terminal
            .write(crate::core::agent_prompt::submit_bytes(prompt));
    }

    pub fn run_command_line(&self, cmd: &str) {
        self.terminal.write(format!("{cmd}\r").into_bytes());
    }

    pub fn shell_spec(&self) -> Option<ShellSpec> {
        self.shell_spec.clone()
    }

    pub fn ssh_spec(&self) -> Option<Box<crate::daemon::protocol::NativeSshSpec>> {
        self.ssh_spec.clone()
    }

    pub fn ssh_phase(&self) -> Option<crate::daemon::protocol::SshPhase> {
        self.terminal.ssh_phase()
    }

    pub fn ssh_disconnected(&self) -> bool {
        self.ssh_spec.is_some() && self.terminal.exited
    }

    fn handle_event(&mut self, ev: AlacEvent, cx: &mut Context<Self>) {
        self.terminal.poll_exited();
        self.sync_typeahead_owner();
        if self.terminal.has_pending_auth() {
            cx.emit(AuthPromptReady);
        }
        match ev {
            AlacEvent::Wakeup => {
                // The grid moved under whatever the search bar last measured.
                self.note_output_under_search(cx);
                cx.notify();
            }
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
                self.title = if self.workspace().is_some() && !self.terminal.child_exited() {
                    "tty7 — disconnected".to_string()
                } else {
                    "tty7 — process exited".to_string()
                };
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
                BellMode::None => {}
                BellMode::Visual => self.flash_bell(cx),
                BellMode::Audible => {
                    if !ring_system_bell() {
                        self.flash_bell(cx);
                    }
                }
                BellMode::Both => {
                    ring_system_bell();
                    self.flash_bell(cx);
                }
            },
            AlacEvent::TextAreaSizeRequest(fmt) => {
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

    fn report_focus_change(&self, focused: bool) {
        let mode = *self.terminal.term.lock().mode();
        if let Some(bytes) = focus_report_bytes(mode, focused) {
            self.terminal.write(bytes);
        }
    }

    fn on_key_down(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let link_dropped_on_a_remote_pane =
            self.workspace().is_some() && !self.terminal.child_exited();
        if self.terminal.exited && !link_dropped_on_a_remote_pane {
            return;
        }
        if self.integration_notice.take().is_some() {
            cx.notify();
        }
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

        if self.search.is_some() && self.search_focused {
            if ks.key == "escape" {
                self.close_search(window, cx);
                cx.stop_propagation();
            }
            return;
        }

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

        if !self.accepts_input(cx) {
            return;
        }

        #[cfg(target_os = "macos")]
        if !window.has_pending_keystrokes() && super::input::defer_to_ime(ks, self.key_flags()) {
            return;
        }

        if self.input_active() {
            self.handle_editor_key(ks, cx);
            cx.stop_propagation();
            return;
        }

        if m.control
            && !m.platform
            && !m.alt
            && ks.key == "r"
            && cx.global::<Config>().history_search
        {
            self.note_integration_gap(cx);
        }

        if m.control && !m.platform && !m.alt && ks.key == "c" && self.handoff_active() {
            // A Tab/unknown-chord handoff leaves the daemon at_prompt: the shell
            // never saw Enter, so there is no C mark. Ctrl-C makes readline draw
            // a fresh prompt whose A/B report is consequently true -> true and
            // does not advance prompt_cycle. Remember this report boundary so
            // that fresh prompt can still return ownership to the local editor.
            self.editor_handoff_interrupt_seq = Some(self.terminal.prompt_seq());
        }

        let kitty = self.key_flags();
        if let Some(bytes) = super::input::keystroke_to_bytes(ks, kitty) {
            let plain = !m.control && !m.alt && !m.platform;
            let interrupt = is_typeahead_interrupt(ks.key.as_str(), m);
            let shell_owns_prompt = self.shell_owns_prompt();
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
                if !shell_owns_prompt && interrupt {
                    // Ctrl-C cancels the foreground input transaction. Clear
                    // the gap before delivering it so a prompt transition
                    // cannot flush this interrupt as a later Ctrl-U.
                    self.observe_typeahead(RawInput::Interrupt);
                }
                self.terminal.write(bytes);
                if !shell_owns_prompt && !interrupt {
                    self.observe_typeahead(RawInput::Key {
                        key: ks.key.as_str(),
                        plain,
                    });
                }
            }
            self.cursor_visible = true;
            self.jump_to_prompt();
            cx.notify();
            cx.stop_propagation();
        }
    }

    fn send_shortcut_bytes(&mut self, bytes: &[u8], key: &str, cx: &mut Context<Self>) {
        let shell_owns_prompt = self.shell_owns_prompt();
        self.release_hold();
        self.send_to_pty(bytes, cx);
        if !shell_owns_prompt {
            let alt = self.on_alt_screen();
            self.typeahead
                .observe(RawInput::Key { key, plain: false }, alt);
        }
    }

    fn handle_cmd_shortcut(
        &mut self,
        ks: &gpui::Keystroke,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> CmdKey {
        let m = &ks.modifiers;
        match ks.key.as_str() {
            "c" => {
                if self.copy_contextual(m.control, cx) {
                    CmdKey::Consumed
                } else {
                    CmdKey::FallThrough
                }
            }
            "x" => {
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
            "a" => {
                self.select_all_contextual(cx);
                CmdKey::Consumed
            }
            "z" => {
                self.undo_edit(m.shift, cx);
                CmdKey::Consumed
            }
            "left" => {
                if self.input_active() {
                    self.editor_move_edge(false, m.shift);
                    cx.notify();
                } else if cfg!(target_os = "macos") && self.accepts_input(cx) {
                    self.send_shortcut_bytes(&[0x01], "a", cx);
                }
                CmdKey::Consumed
            }
            "right" => {
                if self.input_active() {
                    self.editor_move_edge(true, m.shift);
                    cx.notify();
                } else if cfg!(target_os = "macos") && self.accepts_input(cx) {
                    self.send_shortcut_bytes(&[0x05], "e", cx);
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
                } else if cfg!(target_os = "macos") && self.accepts_input(cx) {
                    self.send_shortcut_bytes(&[0x15], "u", cx);
                }
                CmdKey::Consumed
            }
            "delete" => {
                if self.input_active() {
                    if !self.cmd.delete_selection() {
                        self.cmd.delete_to_end();
                    }
                    self.close_completion();
                    self.cursor_visible = true;
                    cx.notify();
                } else if cfg!(target_os = "macos") && self.accepts_input(cx) {
                    self.send_shortcut_bytes(&[0x0b], "k", cx);
                }
                CmdKey::Consumed
            }
            _ => CmdKey::Bubble,
        }
    }

    fn handle_editor_key(&mut self, ks: &gpui::Keystroke, cx: &mut Context<Self>) {
        let m = &ks.modifiers;
        let key = ks.key.as_str();
        self.cursor_visible = true;
        self.jump_to_prompt();

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

        if key != "up" && key != "down" {
            self.editor_goal_col = None;
        }
        if !(m.alt && key == ".") {
            self.last_word_nav = None;
        }

        if self.reverse_search.is_some() {
            self.handle_reverse_search_key(ks, cx);
            return;
        }

        if m.control && !m.platform && !m.alt && matches!(key, "j" | "m") {
            self.accept_line(cx);
            return;
        }

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

        self.close_completion();

        if m.control && !m.platform && !m.alt {
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
            if cfg!(not(target_os = "macos")) && key == "a" {
                self.cmd.select_all();
                self.close_completion();
                self.cursor_visible = true;
                cx.notify();
                return;
            }
            if key == "r" && !cx.global::<Config>().history_search {
                self.handoff_line_to_shell(&[0x12], cx);
                return;
            }
            if self.apply_readline_ctrl(key) {
                cx.notify();
            } else if let Some(bytes) = super::input::keystroke_to_bytes(ks, self.key_flags()) {
                self.handoff_line_to_shell(&bytes, cx);
            } else {
                cx.notify();
            }
            return;
        }

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
                self.submit_command(cx);
                return;
            }
            "backspace" => {
                if self.cmd.is_empty() {
                    self.terminal.write(vec![0x7f]);
                    self.observe_typeahead(RawInput::Key {
                        key: "backspace",
                        plain: true,
                    });
                    return;
                }
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
                if self.editor_move_v(false, m.shift) {
                    cx.notify();
                } else {
                    self.history_prev(cx);
                }
                return;
            }
            "down" => {
                if self.editor_move_v(true, m.shift) {
                    cx.notify();
                } else {
                    self.history_next(cx);
                }
                return;
            }
            "escape" => {
                let bytes = super::input::keystroke_to_bytes(ks, self.key_flags())
                    .unwrap_or_else(|| vec![0x1b]);
                self.terminal.write(bytes);
                return;
            }
            _ => {
                if !m.control && !m.platform && !m.alt {
                    if let Some(ch) = ks.key_char.as_deref() {
                        if !ch.is_empty() && ch.chars().all(|c| c >= '\u{20}' && c != '\u{7f}') {
                            self.commit_text(ch, cx);
                            return;
                        }
                    }
                }
                if m.alt && !m.control && !m.platform && key.chars().count() == 1 {
                    let bytes = super::input::keystroke_to_bytes(ks, self.key_flags())
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

    fn apply_readline_ctrl(&mut self, key: &str) -> bool {
        match key {
            "r" => self.start_reverse_search(),
            "a" => {
                self.cmd.clear_selection();
                self.cmd.move_home();
            }
            "e" => {
                if self.cmd.selection().is_none()
                    && let Some(full) = self.ghost_suggestion()
                {
                    self.cmd.set(&full);
                } else {
                    self.cmd.clear_selection();
                    self.cmd.move_end();
                }
            }
            "b" => {
                self.cmd.clear_selection();
                self.cmd.move_left();
            }
            "f" => {
                if let Some(full) = self.ghost_suggestion() {
                    self.cmd.set(&full);
                } else {
                    self.cmd.clear_selection();
                    self.cmd.move_right();
                }
            }
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
            "y" => self.cmd.yank(),
            "l" => {
                self.terminal.write(vec![0x0c]);
            }
            "c" => {
                self.cmd.clear();
                self.history_nav = None;
                let _ = self.typeahead.drain();
                let _ = self.hold.engage();
                self.terminal.write(vec![0x03]);
            }
            "d" => {
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

    fn editor_move_v(&mut self, down: bool, shift: bool) -> bool {
        let Some((_, scol)) = self.cursor_cell() else {
            return false;
        };
        let cols = self.terminal.term.lock().columns().max(1);
        let chars: Vec<char> = self.cmd.text().chars().collect();
        let len = chars.len();
        let (positions, _r, _c) = input_char_positions(&chars, scol, cols);
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
        if (down && cur_row >= max_row) || (!down && cur_row == 0) {
            self.editor_goal_col = None;
            return false;
        }
        let target = if down { cur_row + 1 } else { cur_row - 1 };
        let goal = *self.editor_goal_col.get_or_insert(cur_col);
        let mut best: Option<(usize, usize)> = None;
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

    fn any_selection(&self) -> bool {
        self.has_selection() || (self.input_active() && self.cmd.selected_text().is_some())
    }

    pub(super) fn key_flags(&self) -> super::input::KeyFlags {
        super::input::KeyFlags::from_mode(self.terminal.term.lock().mode())
    }

    fn tab_bytes(&self, shift: bool) -> Vec<u8> {
        super::input::tab_bytes(shift, self.key_flags())
    }

    fn jump_to_prompt(&mut self) {
        self.cancel_scroll_anim();
        let mut term = self.terminal.term.lock();
        term.selection = None;
        term.scroll_display(Scroll::Bottom);
        drop(term);
        self.scroll_frac = 0.;
    }

    fn send_to_pty(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        if self.terminal.exited || !self.accepts_input(cx) {
            return;
        }
        self.terminal.write(bytes.to_vec());
        self.cursor_visible = true;
        self.jump_to_prompt();
        cx.notify();
    }

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

    pub fn select_all_contextual(&mut self, cx: &mut Context<Self>) {
        if self.input_active() {
            self.cmd.select_all();
            cx.notify();
        } else {
            self.select_all(cx);
        }
    }

    pub fn paste(&mut self, text: String, cx: &mut Context<Self>) {
        if !self.accepts_input(cx) {
            return;
        }
        // Same reason as `commit_text`: what is pasted lands on the prompt, so
        // the prompt is what has to be on screen. Neither branch below moved
        // the viewport, and a paste is a bigger change than a keystroke to
        // make out of sight. This also clears the selection, which the tail of
        // this function used to do on its own.
        self.jump_to_prompt();
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
        let bracketed = self
            .terminal
            .term
            .lock()
            .mode()
            .contains(TermMode::BRACKETED_PASTE);
        self.write_gap_text(&text, paste_bytes(&text, bracketed), cx);
        cx.notify();
    }

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

    pub fn scroll(&mut self, lines: i32, mods: &Modifiers, cx: &mut Context<Self>) {
        if lines == 0 {
            return;
        }
        self.cancel_scroll_anim();
        let mut mode = *self.terminal.term.lock().mode();
        if !self.report_mouse {
            mode.remove(TermMode::MOUSE_MODE);
        }
        match wheel_route(mode, mods.shift, lines > 0) {
            WheelRoute::Report { base } => {
                let (col, row) = self.last_mouse_cell.unwrap_or((0, 0));
                for _ in 0..lines.unsigned_abs() {
                    self.write_mouse(base, mods, col, row, true);
                }
            }
            WheelRoute::Arrows { seq } => {
                let mut out = Vec::with_capacity(seq.len() * lines.unsigned_abs() as usize);
                for _ in 0..lines.unsigned_abs() {
                    out.extend_from_slice(seq);
                }
                self.terminal.write(out);
            }
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

    pub fn copy_selection(&mut self, cx: &mut Context<Self>) {
        let text = self.terminal.term.lock().selection_to_string();
        if let Some(mut text) = text {
            if cx.global::<Config>().clipboard_trim_trailing_spaces {
                text = trim_trailing_spaces(&text);
            }
            if !text.is_empty() {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
            }
        }
    }

    pub fn copy_contextual(&mut self, clear_on_copy: bool, cx: &mut Context<Self>) -> bool {
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

    pub fn find_step(&mut self, forward: bool, cx: &mut Context<Self>) {
        let direction = if forward {
            Direction::Right
        } else {
            Direction::Left
        };
        self.step_match(direction, cx);
    }

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

    fn paste_clipboard_image(&mut self, img: &gpui::Image, cx: &mut Context<Self>) {
        if self.paste_clipboard_image_as_path(img, cx) {
            return;
        }
        self.terminal.write(vec![0x16]);
        self.terminal.term.lock().selection = None;
        cx.notify();
    }

    /// Stage the clipboard image and paste a path for it, answering whether
    /// this took the paste over. `false` leaves the caller forwarding SYN —
    /// see [`stages_clipboard_image`] for when that is the better path.
    fn paste_clipboard_image_as_path(&mut self, img: &gpui::Image, cx: &mut Context<Self>) -> bool {
        let is_remote =
            remote_paste_spec(self.workspace.as_ref(), self.ssh_spec.as_deref()).is_some();
        if !stages_clipboard_image(is_remote) {
            return false;
        }
        let Some(path) = write_clipboard_image(img) else {
            return false;
        };
        // SSH panes can't see the local temp file, so the image is uploaded and
        // the *remote* path pasted instead. Every step of that needs a blocking
        // daemon round trip, which a keystroke handler must not do, so the
        // remote pane pastes from a background task and this returns without
        // touching the line.
        if self.upload_image_for_remote(&path, cx) {
            return true;
        }
        // The upload declined: a WSL pane, which needs a rewrite rather than a
        // transfer, or a workspace with no SSH spec to piggyback on. A macOS
        // pane only reaches this line when it is remote — a local one returned
        // above — so pasting the path is right on every platform, and staying
        // silent here would be the very no-op this route exists to avoid.
        let shares_localhost = self
            .workspace
            .as_ref()
            .is_some_and(|w| w.shares_localhost());
        let path = staged_path_for_pane(&path.to_string_lossy(), shares_localhost);
        let text = shell_escape_path(&path);
        self.paste(format!("{text} "), cx);
        true
    }

    /// Upload a locally staged clipboard image to the pane's remote host and
    /// paste the remote path, all off the UI thread. Answers whether this pane
    /// took the paste over; `false` means a local, WSL, or spec-less pane the
    /// caller should paste the local path for.
    ///
    /// The upload itself still outlives the paste — it has to, or Ctrl+V would
    /// stall on the wire — so the job is watched to completion and a failure
    /// at any point warns the user that the path they were handed is dangling.
    fn upload_image_for_remote(&mut self, local: &std::path::Path, cx: &mut Context<Self>) -> bool {
        use crate::daemon::protocol::{SftpTransferKind, SftpTransferSpec};
        let Some(spec) = remote_paste_spec(self.workspace.as_ref(), self.ssh_spec.as_deref())
        else {
            return false;
        };
        let host = format!("{}@{}", spec.user, spec.host);
        // The only caller stages through `write_clipboard_image`, so this
        // holds; a name that could not stand alone as a remote path component
        // would be a bug worth failing on rather than joining blindly.
        let name = local
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|n| crate::daemon::ssh::sftp::safe_local_name(n));
        let Some(name) = name else {
            log::warn!("refusing to upload a clipboard image named {local:?}");
            return false;
        };
        let route = crate::ui::sftp::SftpRoute::new(self.pane_id, self.workspace.clone());
        let cached = self.remote_clipboard_dir.clone();
        let local = local.to_path_buf();
        let pane_id = self.pane_id;
        cx.spawn(async move |this, cx| {
            let prepared = match cached {
                Some(dir) => Ok(dir),
                None => {
                    let route = route.clone();
                    cx.background_spawn(async move { prepare_remote_clipboard_dir(&route) })
                        .await
                }
            };
            let dir = match this.update(cx, |view, _| {
                view.remote_clipboard_dir = staging_cache(&prepared);
                view.remote_clipboard_dir.clone()
            }) {
                Ok(Some(dir)) => dir,
                Ok(None) => {
                    let reason = prepared.unwrap_or_else(|e| e);
                    Self::paste_local_image_path(&this, cx, &local, &host, &reason);
                    return;
                }
                Err(_) => return,
            };
            let remote = crate::daemon::ssh::sftp::remote_join(&dir, &name);
            let started = {
                let (route, remote, local) = (route.clone(), remote.clone(), local.clone());
                cx.background_spawn(async move {
                    route.transfer_start(SftpTransferSpec {
                        pane_id,
                        kind: SftpTransferKind::Upload,
                        local,
                        remote,
                        recursive: false,
                    })
                })
                .await
            };
            let job = match started {
                Ok(job) => job,
                Err(reason) => {
                    Self::paste_local_image_path(&this, cx, &local, &host, &reason);
                    return;
                }
            };
            let text = shell_escape_path(&remote);
            if this
                .update(cx, |view, cx| view.paste(format!("{text} "), cx))
                .is_err()
            {
                return;
            }
            if let Err(reason) = Self::watch_upload(route, job, &remote, cx).await {
                let _ = this.update_in(cx, |view, window, cx| {
                    view.warn_image_upload_failed(&host, &reason, window, cx);
                });
            }
        })
        .detach();
        true
    }

    /// Fall back to the local path when the remote staging directory cannot be
    /// prepared — the paste is never dropped — and say why it is local.
    fn paste_local_image_path(
        this: &gpui::WeakEntity<Self>,
        cx: &mut gpui::AsyncApp,
        local: &std::path::Path,
        host: &str,
        reason: &str,
    ) {
        let text = shell_escape_path(&local.to_string_lossy());
        let _ = this.update_in(cx, |view, window, cx| {
            view.paste(format!("{text} "), cx);
            view.warn_image_upload_failed(host, reason, window, cx);
        });
    }

    /// Poll a started upload to a terminal state. The transfer history the
    /// SFTP panel reads is only polled while that panel is open, and the
    /// daemon drops finished jobs after 30s, so a paste that no one is
    /// watching would otherwise fail in silence.
    async fn watch_upload(
        route: crate::ui::sftp::SftpRoute,
        job: u64,
        remote: &str,
        cx: &mut gpui::AsyncApp,
    ) -> Result<(), String> {
        use crate::daemon::protocol::{SftpJobState, SftpOp};
        // Long enough for a screenshot over a slow link, bounded so a wedged
        // job cannot poll forever.
        const POLL: std::time::Duration = std::time::Duration::from_millis(500);
        const POLLS: usize = 600;
        for _ in 0..POLLS {
            cx.background_executor().timer(POLL).await;
            let listed = {
                let route = route.clone();
                cx.background_spawn(async move { route.transfer_list() })
                    .await
            };
            // A poll that failed says nothing about the job — keep asking
            // until it answers or the budget above runs out.
            let Ok(listed) = listed else { continue };
            let Some(progress) = listed.into_iter().find(|j| j.job_id == job) else {
                // Pruned after the retention window, or the daemon restarted:
                // there is nothing left to report either way.
                return Ok(());
            };
            match progress.state {
                SftpJobState::Running => continue,
                SftpJobState::Done => {
                    // The staging directory is already owner-only, so this is
                    // belt and braces against a wider umask on the remote.
                    let (route, path) = (route.clone(), remote.to_string());
                    cx.background_spawn(
                        async move { route.op(SftpOp::Chmod { path, mode: 0o600 }) },
                    )
                    .await;
                    return Ok(());
                }
                SftpJobState::Cancelled => return Ok(()),
                SftpJobState::Error => {
                    return Err(progress.error.unwrap_or_else(|| "upload failed".into()));
                }
            }
        }
        Ok(())
    }

    /// One notification per failed paste — the pane's line already has a path
    /// in it, and the user is the only one who can tell whether it matters.
    fn warn_image_upload_failed(
        &self,
        host: &str,
        reason: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        log::warn!("clipboard image upload to {host} failed: {reason}");
        window.push_notification(
            crate::ui::i18n::t_fmt(
                crate::ui::i18n::L10nKey::SftpImagePasteUploadFailed,
                &[("host", host), ("error", reason)],
            ),
            cx,
        );
    }

    pub fn clear_scrollback(&mut self, cx: &mut Context<Self>) {
        use alacritty_terminal::vte::ansi::{ClearMode, Handler as _};

        self.cancel_scroll_anim();
        // Go through `clear_screen` rather than `grid_mut().clear_history()`:
        // it drops a selection anchored in the rows we are about to discard and
        // clamps the vi cursor back into the grid, which purging the history
        // behind the term's back would leave pointing at rows that no longer
        // exist.
        self.terminal.term.lock().clear_screen(ClearMode::Saved);
        // Image placements are anchored in absolute scrollback rows, so the
        // rows we just discarded moved every anchor. Drop them; the daemon does
        // not replay out-of-band image frames, so a browser redraws on its next
        // transmit (same reasoning as the reattach path in `adopt_relink`).
        self.terminal.images().clear();
        self.scroll_frac = 0.;
        self.terminal.write(vec![0x0c_u8]);
        cx.notify();
    }

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

    pub fn set_font_family_bold(&mut self, family: Option<String>, cx: &mut Context<Self>) {
        self.font_bold = self.alt_font(family);
        cx.notify();
    }

    pub fn set_font_family_italic(&mut self, family: Option<String>, cx: &mut Context<Self>) {
        self.font_italic = self.alt_font(family);
        cx.notify();
    }

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

    fn poll_foreground(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.terminal.exited {
            return;
        }
        let at_prompt = self.terminal.at_prompt();

        if self
            .pending_history
            .as_ref()
            .is_some_and(|p| at_prompt && self.terminal.prompt_seq() > p.seq)
        {
            self.flush_pending_history();
            cx.notify();
        }

        if let Some(cwd) = self.cwd()
            && self.ranked_cwd.as_ref() != Some(&cwd)
        {
            self.rerank_history(Some(&cwd));
        }

        if self.integration_notice.is_some() && self.terminal.shell_active() {
            self.integration_notice = None;
            cx.notify();
        }

        if at_prompt != self.last_at_prompt {
            self.last_at_prompt = at_prompt;
            cx.notify();
        }

        let notify_allowed = match cx.global::<Config>().notify_on_command_finish {
            NotifyMode::Never => false,
            NotifyMode::Unfocused => !window.is_window_active(),
            NotifyMode::Always => true,
        };

        let running = !at_prompt;
        if running && self.running_agent.is_none() {
            self.running_agent = self.terminal.foreground_agent();
        }
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
                        Some(_) if self.agent_was_rich => {}
                        Some(agent) => self.notify_agent_finished(agent, elapsed, cx),
                        None => {
                            let threshold = std::time::Duration::from_secs(
                                cx.global::<Config>().notify_threshold_secs,
                            );
                            if elapsed >= threshold {
                                self.notify_command_finished(&title, elapsed, cx);
                            }
                        }
                    }
                }
            }
            _ => {}
        }

        let turn_finished = self.poll_agent_status(notify_allowed, cx);

        let session = self.terminal.agent_session();
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
            if cmd_finished || turn_finished {
                self.mark_repo_changed(cwd_now.as_deref(), cx);
            }
            self.refresh_git_status(cwd_now, GitRefresh::Edge, cx);
        } else if tool_activity {
            self.refresh_git_status(cwd_now, GitRefresh::Opportunistic, cx);
        }

        self.follow_history_scope(cx);
    }

    /// Tell the source control cache that a command just ran here.
    ///
    /// The `.git` watch catches anything that writes the repository, and the
    /// file tree catches edits in the directories it is showing. What is left
    /// is the common case neither sees: a command that edits a file somewhere
    /// the tree is not looking. A command boundary is the cheapest honest
    /// signal that that may have happened.
    ///
    /// Only the epoch moves. Scheduling the debounced re-read needs the app
    /// entity, which a pane does not hold — but `refresh_git_status` below
    /// writes `GitStatusCache`, the app observes that global, and the panel's
    /// next render finds the repository stale and asks. One notify, not two.
    fn mark_repo_changed(&self, cwd: Option<&std::path::Path>, cx: &mut Context<Self>) {
        use crate::terminal::git_data::ScmData;
        use crate::terminal::git_status::GitStatusCache;

        let Some(cwd) = cwd else { return };
        let Some(root) = cx
            .try_global::<GitStatusCache>()
            .and_then(|cache| cache.repo_root_for(self.host_id, cwd))
            .map(std::path::Path::to_path_buf)
        else {
            return;
        };
        cx.default_global::<ScmData>().bump(self.host_id, &root);
    }

    fn desired_history_scope(&self) -> super::history::Scope {
        if let Some(ctx) = self.remote_context() {
            return super::history::Scope::remote(&ctx.target);
        }
        if !self.host_id.is_local() {
            return super::history::Scope::remote(&format!("host-{:016x}", self.host_id.0));
        }
        super::history::Scope::Local
    }

    fn follow_history_scope(&mut self, cx: &mut Context<Self>) {
        let scope = self.desired_history_scope();
        if scope == self.history_scope {
            return;
        }
        self.flush_pending_history();
        self.history_scope = scope.clone();
        self.history.clear();
        self.history_counts.clear();
        self.history_cwds.clear();
        self.history_meta.clear();
        self.history_ranked.clear();
        self.history_frecency.clear();
        self.history_nav = None;
        self.reverse_search = None;
        cx.notify();

        let shell_files = self.remote_shell_history_sources(cx);
        let loading = scope.clone();
        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_spawn(async move {
                    let files = shell_files
                        .into_iter()
                        .filter_map(|(host, path)| {
                            // The name comes along: it is what tells the loader
                            // which shell's format the bytes are in.
                            let name = path.file_name()?.to_string_lossy().into_owned();
                            Some((name, host.read_file(&path, MAX_HISTORY_BYTES).ok()?))
                        })
                        .collect();
                    super::history::load_with_shell_files(&loading, files)
                })
                .await;
            this.update(cx, |view, cx| {
                if view.history_scope != scope {
                    return;
                }
                view.history = loaded.entries;
                view.history_counts = loaded.counts;
                view.history_cwds = loaded.cwds;
                view.history_meta = loaded.meta;
                let cwd = view.ranked_cwd.clone();
                view.rerank_history(cwd.as_deref());
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn remote_shell_history_sources(
        &self,
        cx: &mut Context<Self>,
    ) -> Vec<(crate::ui::host_ops::SharedHost, std::path::PathBuf)> {
        if self.history_scope.is_local() || self.host_id.is_local() {
            return Vec::new();
        }
        // The Host reaches the workspace machine's home directory and nothing
        // beyond it. A pane that has ssh'ed onward from there (remote_context)
        // is scoped to the *inner* target, and seeding that scope from the
        // workspace host's ~/.zsh_history would offer commands from the wrong
        // box — the exact confusion scoping exists to prevent. Those panes
        // start from what tty7 recorded for the inner target, like bare ssh.
        if self.remote_context().is_some() {
            return Vec::new();
        }
        let Some(host) = self.host(cx) else {
            return Vec::new();
        };
        if !host.is_connected() {
            return Vec::new();
        }
        let Some(home) = crate::ui::remote_connect::HostLinks::home(cx, self.host_id) else {
            return Vec::new();
        };
        super::history::shell_history_names()
            .into_iter()
            .map(|name| (std::sync::Arc::clone(&host), host.join(&home, name)))
            .collect()
    }

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
        cx.default_global::<GitStatusCache>();
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
        crate::ui::host_ops::HostOps::run_detached(
            host,
            cx,
            move |h| crate::terminal::git_status::probe(h, &probe_cwd),
            move |cx, result| {
                let rerun = cx.update_global::<GitStatusCache, _>(|cache, _| {
                    cache.finish_probe(id, &cwd, result)
                });
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

    fn poll_agent_status(&mut self, notify_allowed: bool, cx: &mut Context<Self>) -> bool {
        use crate::core::cli_agent::AgentStatus;

        let session = self.terminal.agent_session();
        if session.as_ref().is_some_and(|s| s.rich) {
            self.agent_was_rich = true;
        }
        if self.terminal.foreground_agent().is_none() && session.is_none() {
            self.agent_was_rich = false;
        }

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
                    .unwrap_or_else(|| t(L10nKey::NotifyAgentWaiting).to_string());
                self.notify_pane(Some(agent_name), &body, cx);
            }
            Some(AgentStatus::Done)
                if rich
                    && notify_allowed
                    && matches!(
                        prev,
                        Some(AgentStatus::Working) | Some(AgentStatus::Waiting)
                    ) =>
            {
                let body = match self.agent_turn_started.take() {
                    Some(start) => {
                        let secs = start.elapsed().as_secs().to_string();
                        t_fmt(L10nKey::NotifyAgentFinished, &[("secs", &secs)])
                    }
                    None => t(L10nKey::NotifyTurnFinished).to_string(),
                };
                self.notify_pane(Some(agent_name), &body, cx);
            }
            _ => {}
        }
        cx.notify();
        turn_finished
    }

    fn at_shell_prompt(&self) -> bool {
        self.terminal.at_prompt()
    }

    fn cursor_cell(&self) -> Option<(usize, usize)> {
        let term = self.terminal.term.lock();
        let content = term.renderable_content();
        let row = content.cursor.point.line.0 + content.display_offset as i32;
        let col = content.cursor.point.column.0;
        (row >= 0).then_some((row as usize, col))
    }

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
            1 if shift => {
                self.cmd.extend_to(idx);
                self.editor_selecting = true;
                self.editor_drag_word = None;
            }
            1 => {
                self.cmd.set_cursor(idx);
                self.cmd.clear_selection();
                self.editor_selecting = true;
                self.editor_drag_word = None;
            }
            2 => {
                let cfg = cx.global::<Config>();
                let (seps, smart) = (cfg.word_separators.clone(), cfg.smart_select);
                self.cmd.select_word_at(idx, &seps, smart);
                self.editor_selecting = true;
                self.editor_drag_word = self.cmd.selection();
            }
            _ => {
                self.cmd.select_all();
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

    pub fn editor_drag(&mut self, col: usize, row: usize, cx: &mut Context<Self>) -> bool {
        if !self.editor_selecting {
            return false;
        }
        let Some(idx) = self.editor_char_index(col, row, true) else {
            return false;
        };
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

    pub fn input_active(&self) -> bool {
        self.input_inactive_reason().is_none()
    }

    fn input_inactive_reason(&self) -> Option<&'static str> {
        if self.terminal.exited {
            return Some("the shell has exited");
        }
        if self.search_focused {
            return Some("the search field holds the keyboard");
        }
        if self.on_alt_screen() {
            return Some("the pane is on the alternate screen");
        }
        if self.shell_vi_prompt() {
            return Some("the shell prompt is in vi mode");
        }
        if self.handoff_active() {
            return Some("this prompt's line was already handed to the shell");
        }
        if !self.at_shell_prompt() {
            return Some("the shell has not reported a prompt (no OSC 133)");
        }
        None
    }

    fn link_inactive_reason(&self, cx: &gpui::App) -> Option<&'static str> {
        (!self.accepts_input(cx)).then_some("the remote link is not attached")
    }

    fn shell_vi_prompt(&self) -> bool {
        self.terminal.shell_vi_mode() && self.terminal.at_prompt() && !self.on_alt_screen()
    }

    fn handoff_active(&self) -> bool {
        self.editor_handoff == Some(self.terminal.prompt_cycle())
            && self
                .editor_handoff_interrupt_seq
                .is_none_or(|seq| self.terminal.prompt_seq() <= seq)
            && self.terminal.at_prompt()
            && !self.on_alt_screen()
    }

    fn shell_owns_prompt(&self) -> bool {
        self.shell_vi_prompt() || self.handoff_active()
    }

    fn on_alt_screen(&self) -> bool {
        self.terminal
            .term
            .lock()
            .mode()
            .contains(TermMode::ALT_SCREEN)
    }

    fn typeahead_blocked(&self) -> bool {
        self.on_alt_screen()
            || self.terminal.foreground_agent().is_some()
            || self.terminal.agent_session().is_some()
    }

    fn sync_typeahead_owner(&mut self) {
        let blocked = self.typeahead_blocked();
        sync_typeahead_owner_state(
            &mut self.typeahead,
            &mut self.last_typeahead_blocked,
            blocked,
        );
    }

    fn observe_typeahead(&mut self, input: RawInput<'_>) {
        // The input that crosses an ownership boundary belongs to neither side.
        let blocked = self.typeahead_blocked();
        observe_typeahead_for_owner(
            &mut self.typeahead,
            &mut self.last_typeahead_blocked,
            input,
            blocked,
        );
    }

    fn flush_typeahead(&mut self) {
        let Some(seed) = self.typeahead.drain() else {
            return;
        };
        self.terminal.write(vec![0x15]);
        if !seed.is_empty() {
            self.cmd.prepend_str(&seed);
        }
    }

    fn wipe_pending_typeahead(&mut self) {
        if self.typeahead.drain().is_some() {
            self.terminal.write(vec![0x15]);
        }
    }

    fn gap_holdable(&self) -> bool {
        self.terminal.shell_active() && !self.on_alt_screen() && !self.shell_owns_prompt()
    }

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
            self.release_hold();
        }
        self.terminal.write(bytes);
        self.observe_typeahead(RawInput::Text(text));
    }

    fn release_hold(&mut self) {
        if let Some((net, bytes)) = self.hold.release() {
            self.terminal.write(bytes);
            self.observe_typeahead(RawInput::Text(&net));
        }
    }

    fn arm_hold_timer(&mut self, epoch: u64, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(HOLD_WINDOW).await;
            let _ = this.update(cx, |view, cx| view.dump_hold(epoch, cx));
        })
        .detach();
    }

    fn dump_hold(&mut self, epoch: u64, cx: &mut Context<Self>) {
        if !self.accepts_input(cx) {
            let _ = self.hold.timeout(epoch);
            return;
        }
        if let Some((net, bytes)) = self.hold.timeout(epoch) {
            self.terminal.write(bytes);
            self.observe_typeahead(RawInput::Text(&net));
            cx.notify();
        }
    }

    fn insert_newline_action(&mut self, cx: &mut Context<Self>) {
        if !self.input_active() || self.reverse_search.is_some() {
            cx.propagate();
            return;
        }
        self.jump_to_prompt();
        self.close_completion();
        self.cursor_visible = true;
        self.cmd.insert_str("\n");
        self.history_nav = None;
        self.editor_goal_col = None;
        self.last_word_nav = None;
        cx.notify();
    }

    fn insert_newline_fallback_action(&mut self, cx: &mut Context<Self>) {
        if self.input_active() {
            self.insert_newline_action(cx);
        } else if (self.search.is_some() && self.search_focused)
            || self.key_flags().kitty_active()
            || !self.accepts_input(cx)
        {
            cx.propagate();
        } else {
            self.send_shortcut_bytes(b"\n", "enter", cx);
        }
    }

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

    fn submit_command(&mut self, cx: &mut Context<Self>) {
        if self.terminal.exited || !self.accepts_input(cx) {
            return;
        }
        if let Some(net) = self.hold.engage() {
            self.cmd.prepend_str(&net);
        }
        let line = self.cmd.text();
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

        self.wipe_pending_typeahead();
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

    fn insert_last_word(&mut self, cx: &mut Context<Self>) {
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
        let start = match &resumed {
            Some(walk) => walk.entry.checked_sub(1),
            None => self.history.len().checked_sub(1),
        };
        let Some(mut entry) = start else {
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

        if let Some(walk) = resumed {
            self.cmd.clear_selection();
            self.cmd.set_cursor(walk.at);
            self.cmd.extend_to(walk.at + walk.word.chars().count());
            self.cmd.delete_selection();
        }
        self.cmd.insert_str(&word);
        let at = self.cmd.cursor() - word.chars().count();
        self.last_word_nav = Some(LastWordWalk { entry, at, word });
        self.history_nav = None;
        cx.notify();
    }

    fn history_prev(&mut self, cx: &mut Context<Self>) {
        if self.history.is_empty() {
            return;
        }
        let next = match self.history_nav {
            None => {
                self.history_stash = self.cmd.text();
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.history_nav = Some(next);
        self.cmd.set(&self.history[next]);
        cx.notify();
    }

    fn history_next(&mut self, cx: &mut Context<Self>) {
        let Some(i) = self.history_nav else {
            return;
        };
        if i + 1 < self.history.len() {
            self.history_nav = Some(i + 1);
            self.cmd.set(&self.history[i + 1]);
        } else {
            self.history_nav = None;
            let stash = std::mem::take(&mut self.history_stash);
            self.cmd.set(&stash);
        }
        cx.notify();
    }

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
        super::history::append(&self.history_scope, &p.line, p.cwd.as_deref(), p.ts, exit);
    }

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
        let route = self.pane_route();
        cx.spawn(async move |this, cx| {
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

    fn start_reverse_search(&mut self) {
        if self.reverse_search.is_none() {
            self.reverse_search = Some(ReverseSearch::new(&self.history, &self.history_frecency));
        }
    }

    fn handle_reverse_search_key(&mut self, ks: &gpui::Keystroke, cx: &mut Context<Self>) {
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
                self.reverse_search = None;
                self.cmd.set(&line);
                self.submit_command(cx);
            }
        }
        cx.notify();
    }

    fn handoff_line_to_shell(&mut self, chord: &[u8], cx: &mut Context<Self>) {
        if !self.accepts_input(cx) {
            return;
        }
        if let Some(net) = self.hold.engage() {
            self.cmd.prepend_str(&net);
        }
        let line = self.cmd.text();
        if line.contains('\n') {
            cx.notify();
            return;
        }
        self.close_completion();
        self.wipe_pending_typeahead();
        let tail = line.chars().count().saturating_sub(self.cmd.cursor());
        if !line.is_empty() {
            self.terminal.write(line.into_bytes());
            if tail > 0 {
                let left: &[u8] = if self.key_flags().app_cursor() {
                    b"\x1bOD"
                } else {
                    b"\x1b[D"
                };
                self.terminal.write(left.repeat(tail));
            }
        }
        self.cmd.clear();
        self.editor_handoff = Some(self.terminal.prompt_cycle());
        self.editor_handoff_interrupt_seq = None;
        self.send_to_pty(chord, cx);
    }

    fn tab_pressed(&mut self, forward: bool, cx: &mut Context<Self>) {
        if self.search_focused {
            cx.propagate();
            return;
        }
        if let Some(reason) = self.link_inactive_reason(cx) {
            log::debug!(target: "tty7::completion", "Tab does nothing and the line stays: {reason}");
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

    fn handoff_tab_to_shell(&mut self, shift: bool, cx: &mut Context<Self>) {
        let bytes = self.tab_bytes(shift);
        self.handoff_line_to_shell(&bytes, cx);
    }

    fn complete_tab(&mut self, forward: bool, cx: &mut Context<Self>) {
        if self.reverse_search.is_some() {
            return;
        }
        if !cx.global::<Config>().tab_completion {
            log::debug!(target: "tty7::completion", "handing the line to the shell: tab_completion is off");
            self.handoff_tab_to_shell(!forward, cx);
            return;
        }
        if self.completion.is_some() {
            self.completion_tab_step(forward, cx);
            return;
        }

        let cwd = self
            .paths_are_local()
            .then(|| self.local_cwd().or_else(|| std::env::current_dir().ok()))
            .flatten();
        let share_cwd = if cwd.is_none() {
            self.wsl_share_cwd()
        } else {
            None
        };
        let line = self.cmd.text();
        let cursor = self.cmd.cursor();
        let comp = match &share_cwd {
            Some(share) => super::completion::complete_foreign(&line, cursor, share),
            None => super::completion::complete(&line, cursor, cwd.as_deref()),
        };
        let Some(comp) = comp else {
            if self.spawn_remote_path_completion(&line, cursor, forward, cx) {
                return;
            }
            log::debug!(
                target: "tty7::completion",
                "handing the line to the shell: no candidates for {line:?} at {cursor} \
                 (local cwd {cwd:?}, share cwd {share_cwd:?}, remote cwd {:?})",
                self.remote_ssh_cwd(),
            );
            self.handoff_tab_to_shell(!forward, cx);
            return;
        };

        let pending_generators = comp.pending.len();

        let (word_start, word_end) = match comp.candidates.first() {
            Some(c) => (c.start, c.end),
            None => (word_start_of(&line, cursor), cursor),
        };
        let Some(generation) = self.offer_candidates(
            &line,
            word_start,
            word_end,
            comp.candidates,
            pending_generators,
            cx,
        ) else {
            return;
        };

        let Some(cwd) = cwd else { return };
        for pending in comp.pending {
            let script = pending.script;
            let cwd = cwd.clone();
            cx.spawn(async move |this, cx| {
                let results = cx
                    .background_executor()
                    .spawn(async move { super::generator::run(&script, &cwd) })
                    .await;
                let _ = this.update(cx, |view, cx| {
                    view.completion_merge(generation, results, cx);
                });
            })
            .detach();
        }
    }

    fn offer_candidates(
        &mut self,
        line: &str,
        word_start: usize,
        word_end: usize,
        cands: Vec<completion::Candidate>,
        pending_generators: usize,
        cx: &mut Context<Self>,
    ) -> Option<u64> {
        let has_pending = pending_generators > 0;
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
        let s = CompletionSession::new(word_start, word.clone(), cands, pending_generators);
        if !has_pending
            && let Some(lcp) = s.common_prefix()
            && lcp.chars().count() > word.chars().count()
            && escape_candidate(&lcp) == lcp
        {
            self.apply_candidate(line, word_start, word_end, &lcp);
        }
        let generation = self.open_completion(s);
        self.cursor_visible = true;
        cx.notify();
        Some(generation)
    }

    /// The cwd to list over the distro's `\\wsl$` share, for a pane whose
    /// filesystem is a WSL distro's: the local wsl.exe pane (tagged by its
    /// remote context) and the WSL-workspace pane (tagged by its workspace
    /// target) both report a POSIX cwd this process cannot read natively.
    fn wsl_share_cwd(&self) -> Option<std::path::PathBuf> {
        let distro = wsl_share_distro(
            self.terminal.remote_context().as_ref(),
            self.workspace.as_ref(),
            self.host_id.is_local(),
        )?;
        let cwd = self.cwd()?;
        wsl_share_path(&distro, &cwd.to_string_lossy())
    }

    fn remote_ssh_cwd(&self) -> Option<String> {
        let owned = match self.terminal.remote_context() {
            Some(remote) => remote.kind == crate::daemon::protocol::RemoteKind::NativeSsh,
            // A WSL workspace carries no SSH spec: there is no connection to
            // list over, and its panes complete through the `\\wsl$` share
            // instead — so only a spec-carrying (SSH) workspace claims the
            // remote-listing path.
            None => self.workspace.as_ref().is_some_and(|w| w.spec.is_some()),
        };
        if !owned {
            return None;
        }
        let cwd = self.cwd()?.to_string_lossy().into_owned();
        cwd.starts_with('/').then_some(cwd)
    }

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
        if self.remote_completion_inflight {
            return true;
        }
        self.remote_completion_inflight = true;
        let route = crate::ui::sftp::SftpRoute::new(self.pane_id, self.workspace.clone());
        let dir = req.dir.clone();
        let line = line.to_string();
        log::debug!(target: "tty7::completion", "listing {dir} over the remote's own connection");
        cx.spawn(async move |this, cx| {
            let listed = cx.background_spawn(async move { route.list(&dir) }).await;
            let entries = listed.unwrap_or_else(|e| {
                log::warn!(
                    target: "tty7::completion",
                    "remote listing failed, treating it as no candidates: {e}"
                );
                Vec::new()
            });
            let _ = this.update(cx, |view, cx| {
                view.remote_completion_inflight = false;
                view.remote_path_results(req, &line, cursor, entries, forward, cx);
            });
        })
        .detach();
        true
    }

    fn remote_path_results(
        &mut self,
        req: completion::RemotePathRequest,
        line: &str,
        cursor: usize,
        listed: Vec<crate::daemon::protocol::SftpEntry>,
        forward: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some(reason) = self
            .link_inactive_reason(cx)
            .or_else(|| self.input_inactive_reason())
        {
            log::debug!(
                target: "tty7::completion",
                "dropping a remote listing for {line:?}: {reason}"
            );
            return;
        }
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
        self.offer_candidates(line, req.word_start, req.cursor, cands, 0, cx);
    }

    fn open_completion(&mut self, session: CompletionSession) -> u64 {
        self.completion = Some(session);
        self.completion_generation = self.completion_generation.wrapping_add(1);
        self.completion_generation
    }

    fn close_completion(&mut self) {
        let _ = self.take_completion();
    }

    fn take_completion(&mut self) -> Option<CompletionSession> {
        let s = self.completion.take();
        if s.is_some() {
            self.completion_generation = self.completion_generation.wrapping_add(1);
        }
        s
    }

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
        let spent = match self.completion.as_mut() {
            Some(s) => {
                s.generator_answered();
                s.merge(new, &live_word);
                s.is_spent()
            }
            None => false,
        };
        if spent {
            self.close_completion();
        }
        cx.notify();
    }

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
                    return;
                }
                if escape_candidate(&lcp) == lcp {
                    self.apply_candidate(&line, word_start, cursor, &lcp);
                    self.cursor_visible = true;
                    cx.notify();
                    return;
                }
            }
        }
        self.completion_select(forward, cx);
    }

    fn completion_select(&mut self, forward: bool, cx: &mut Context<Self>) {
        if let Some(s) = self.completion.as_mut() {
            s.select(forward);
            self.cursor_visible = true;
            cx.notify();
        }
    }

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

    fn completion_insert(&mut self, cand: &completion::Candidate, start: usize) {
        let line = self.cmd.text();
        let len = line.chars().count();
        let cursor = self.cmd.cursor().min(len);
        let mut text = escape_candidate(&cand.text);
        if cand.is_dir() {
            if !text.ends_with('/') {
                text.push('/');
            }
        } else if cursor == len {
            text.push(' ');
        }
        self.apply_candidate(&line, start, cursor, &text);
    }

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

    pub fn input_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.commit_text(text, cx);
    }

    pub fn commit_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if self.terminal.exited || text.is_empty() || !self.accepts_input(cx) {
            return;
        }
        // Typing goes to the prompt, so the view has to be looking at it.
        // Only the last branch below used to do this, which is the branch
        // taken when tty7 is *not* driving the line — so scrolling up and
        // typing did the one thing it must never do at a shell prompt:
        // accepted the characters somewhere the user could not see them.
        // `handle_editor_key` has always jumped, so Left and Backspace came
        // back to the prompt and the letters between them did not.
        self.jump_to_prompt();
        if let Some(rs) = self.reverse_search.as_mut() {
            rs.push_query(text, &self.history, &self.history_frecency);
            self.cursor_visible = true;
            cx.notify();
            return;
        }
        if self.input_active() {
            self.cmd.insert_str(text);
            self.history_nav = None;
            self.editor_goal_col = None;
            self.last_word_nav = None;
            self.completion_refilter();
            self.cursor_visible = true;
            cx.notify();
            return;
        }
        self.write_gap_text(text, text.as_bytes().to_vec(), cx);
        self.cursor_visible = true;
        cx.notify();
    }

    pub fn set_marked_text(&mut self, text: String, cx: &mut Context<Self>) {
        self.marked_text = text;
        cx.notify();
    }

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
            2 => SelectionType::Semantic,
            n if n >= 3 => SelectionType::Lines,
            _ => SelectionType::Simple,
        };
        let mut selection = Selection::new(ty, point, side);
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
            return;
        }
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

    fn drag_scroll_tick(&mut self, epoch: u64, cx: &mut Context<Self>) -> bool {
        if epoch != self.drag_scroll_epoch {
            return false;
        }
        if !self.selecting {
            self.drag_scroll = None;
        }
        let Some(ds) = self.drag_scroll else {
            return false;
        };
        self.cancel_scroll_anim();
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

    fn on_scroll(&mut self, ev: &ScrollWheelEvent, window: &mut Window, cx: &mut Context<Self>) {
        // The platform modifier alone turns the wheel into a zoom, the way it
        // does in a browser. Any other modifier alongside it is somebody else's
        // gesture — shift in particular is the escape hatch that scrolls the
        // scrollback out from under a mouse-reporting program.
        if ev.modifiers.secondary() && ev.modifiers.number_of_modifiers() == 1 {
            self.zoom_scroll(ev, window, cx);
            return;
        }
        let mult = cx.global::<Config>().mouse_scroll_multiplier;
        let raw = match ev.delta {
            ScrollDelta::Lines(p) => p.y,
            ScrollDelta::Pixels(p) => p.y.as_f32() / self.line_height.as_f32(),
        };
        let delta = raw * mult;
        let gesturing = self.track_scroll_gesture(ev.touch_phase);

        let quantized = !ev.modifiers.shift && {
            let mode = *self.terminal.term.lock().mode();
            mode.intersects(TermMode::MOUSE_MODE)
                || mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL)
        };
        if quantized {
            // Whole lines are what the application gets told about, so this path
            // cannot be spread over frames.
            self.cancel_scroll_anim();
            let total = self.scroll_debt + delta;
            let lines = total.trunc() as i32;
            self.scroll_debt = total - lines as f32;
            if lines != 0 {
                self.scroll(lines, &ev.modifiers, cx);
            }
            return;
        }

        if self.should_animate_scroll(delta, gesturing, cx) {
            self.queue_scroll_anim(delta, window, cx);
        } else {
            self.cancel_scroll_anim();
            self.smooth_scroll(delta, cx);
        }
    }

    /// Resize the terminal font by whole steps under the platform modifier.
    ///
    /// The event never reaches the buffer, and never reaches the program
    /// running in it either: zooming is chrome, and showing a pane to someone
    /// standing behind you has to work the same whether or not what is running
    /// asked for the wheel. Steps go out as the same actions the keyboard and
    /// the View menu send, so the min/max clamp and the saved setting live in
    /// one place — [`Tty7App::change_font_size`](crate::ui::app::Tty7App).
    fn zoom_scroll(&mut self, ev: &ScrollWheelEvent, window: &mut Window, cx: &mut Context<Self>) {
        // Whatever the scrollback still had in flight is dropped: it was
        // travelling in lines of a font that is about to change size.
        self.cancel_scroll_anim();
        let lines = match ev.delta {
            ScrollDelta::Lines(p) => p.y,
            ScrollDelta::Pixels(p) => p.y.as_f32() / self.line_height.as_f32(),
        };
        let gesturing = self.track_scroll_gesture(ev.touch_phase);
        let (steps, debt) = zoom_scroll_steps(lines, self.zoom_debt, gesturing);
        self.zoom_debt = debt;
        for _ in 0..steps.unsigned_abs() {
            if steps > 0 {
                window.dispatch_action(Box::new(IncreaseFontSize), cx);
            } else {
                window.dispatch_action(Box::new(DecreaseFontSize), cx);
            }
        }
    }

    /// Track whether the pointing device is mid-gesture, which is what tells a
    /// trackpad apart from a wheel.
    ///
    /// Not the delta *type*: macOS reports a wheel mouse as pixels too — one
    /// notch arrives as a single ~100px event — so `Pixels` says nothing about
    /// the device. Phase does: only devices that can gesture ever report
    /// `Started`/`Ended`, and a wheel is `Moved` forever, on every platform.
    ///
    /// The gesture is held open on a timer rather than closed on `Ended`,
    /// because lifting the fingers is not the end of the stream — the momentum
    /// tail keeps delivering `Moved` events, larger than the gesture itself,
    /// and animating those would put a second layer of smoothing on scrolling
    /// the system is already smoothing.
    fn track_scroll_gesture(&mut self, phase: gpui::TouchPhase) -> bool {
        let now = std::time::Instant::now();
        let live = matches!(phase, gpui::TouchPhase::Started)
            || self.gesture_until.is_some_and(|until| now < until);
        self.gesture_until = live.then(|| now + SCROLL_GESTURE_IDLE);
        live
    }

    fn should_animate_scroll(&self, delta: f32, gesturing: bool, cx: &App) -> bool {
        if gesturing || !cx.global::<Config>().smooth_scroll {
            return false;
        }
        // Anything already in flight keeps accumulating, or a slow notch
        // arriving mid-animation would fight the frames still to come.
        self.scroll_anim.is_some() || delta.abs() >= SCROLL_ANIM_MIN_JUMP
    }

    /// Add `delta` to the in-flight animation, starting the frame loop if it is
    /// idle. Successive notches accumulate rather than restart, so spinning the
    /// wheel fast still lands exactly where the notches asked for.
    fn queue_scroll_anim(&mut self, delta: f32, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(anim) = self.scroll_anim.as_mut() {
            anim.remaining += delta;
            return;
        }
        self.scroll_anim = Some(ScrollAnim {
            remaining: delta,
            last: std::time::Instant::now(),
        });
        self.scroll_anim_epoch += 1;
        let epoch = self.scroll_anim_epoch;
        schedule_scroll_anim_frame(cx.weak_entity(), epoch, window);
        // Ask for a frame right away: the callback chain's first step then
        // fires on the next display refresh even on a backend that only draws
        // when the window is dirty, and the callback keeps it dirty from then
        // on. Without this, a wheel notch queued on an idle, unfocused pane
        // could sit dormant instead of spreading over frames.
        cx.notify();
    }

    /// Drop whatever is left to travel. Every other way the display offset moves
    /// — jumping to a prompt, dragging a selection past the edge, the keyboard
    /// and mouse-reporting paths — has to come through here first, or the
    /// animation would keep walking away from wherever it put us.
    fn cancel_scroll_anim(&mut self) {
        if self.scroll_anim.take().is_some() {
            self.scroll_anim_epoch += 1;
        }
    }

    /// Advance the in-flight animation by the ground it covered since the
    /// previous frame. Runs at frame time — right before the next frame is
    /// drawn — so every presented frame shows exactly one decay step and no
    /// step is ever skipped or doubled against the monitor's cadence. Returns
    /// whether the animation is still going.
    fn scroll_anim_frame(&mut self, epoch: u64, cx: &mut Context<Self>) -> bool {
        if epoch != self.scroll_anim_epoch {
            return false;
        }
        let Some(anim) = self.scroll_anim.as_mut() else {
            return false;
        };
        let now = std::time::Instant::now();
        let dt = now.duration_since(anim.last);
        anim.last = now;
        let (step, last) = scroll_anim_step(anim.remaining, dt);
        anim.remaining -= step;
        if last {
            self.scroll_anim = None;
        }
        // Hitting the top or the bottom of the scrollback consumes nothing; the
        // remaining distance has nowhere to go, so stop instead of decaying it
        // against the clamp for another 100ms.
        let moved = self.smooth_scroll(step, cx);
        if !moved {
            self.cancel_scroll_anim();
            return false;
        }
        !last
    }

    /// Apply `delta` lines right now. Returns whether anything actually moved.
    fn smooth_scroll(&mut self, delta: f32, cx: &mut Context<Self>) -> bool {
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
            return true;
        }
        false
    }

    /// Settle up with the scrollback bar for this frame: move the viewport
    /// where a drag asked for, then tell the bar where the grid ended up.
    ///
    /// Both halves belong here rather than in the handle, so the bar — which
    /// runs from a mouse handler, with no pane to call into — never reaches
    /// into the terminal behind the pane's back.
    fn sync_scrollbar(&mut self) {
        if let Some(target) = self.scroll_handle.take_pending() {
            // Whatever the wheel had in flight was heading somewhere else.
            self.cancel_scroll_anim();
            let mut term = self.terminal.term.lock();
            let delta = target as i32 - term.grid().display_offset() as i32;
            if delta != 0 {
                term.scroll_display(Scroll::Delta(delta));
            }
            drop(term);
            // A sub-line remainder left over from a smooth wheel scroll would
            // paint the grid shifted off the row the thumb just picked.
            self.scroll_frac = 0.;
        }
        let term = self.terminal.term.lock();
        let grid = GridScroll {
            history: term.grid().history_size(),
            display_offset: term.grid().display_offset(),
            screen_lines: term.screen_lines(),
            line_height: self.line_height.as_f32(),
        };
        drop(term);
        self.scroll_handle.sync(grid);
    }

    /// The scrollback bar, laid down the right edge of the grid.
    ///
    /// The track is inset to the rows themselves — [`GRID_PAD_Y`] is padding
    /// the grid never scrolls through, and counting it would leave the thumb
    /// short of the ends by that much.
    fn render_scrollbar(&self) -> impl IntoElement + use<> {
        div()
            .absolute()
            .top(px(GRID_PAD_Y))
            .left_0()
            .right_0()
            .h(self.line_height * self.terminal.size().rows as f32)
            // No `scrollbar_show` override: the bar takes `cx.theme()`'s, which
            // `apply_theme` pins to `Scrolling` for every list in the app. A
            // pane disagreeing with the sidebar about when a scrollbar is worth
            // showing would be the odd one out.
            .child(Scrollbar::vertical(&self.scroll_handle).id("terminal-scrollbar"))
    }

    fn grid_line(
        term: &alacritty_terminal::Term<crate::terminal::remote::EventProxy>,
        row: usize,
    ) -> Option<Line> {
        let line = Line(row as i32 - term.grid().display_offset() as i32);
        (line >= term.topmost_line() && line <= term.bottommost_line()).then_some(line)
    }

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
        if matches!(plan, LoopbackPlan::NoForwardNeeded) {
            return LoopbackOpen::NotLoopback;
        }

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

    pub fn hover_link_at(
        &mut self,
        col: usize,
        row: usize,
        include_files: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        self.last_hover_cell = Some((col, row));
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

    pub fn clear_hovered_link(&mut self, cx: &mut Context<Self>) {
        self.last_hover_cell = None;
        if self.hovered_link.take().is_some() {
            cx.notify();
        }
    }

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

        if let Some(hl) = term.grid()[line][Column(col)].hyperlink() {
            let uri = hl.uri().to_string();
            if let Some((start, end)) = super::smart_select::hyperlink_run(&term, click) {
                return Some((LinkTarget::Url(uri), start, end));
            }
        }

        let (text, points, click_idx) = super::smart_select::logical_line_at(&term, click, true)?;
        drop(term);
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

    fn render_input_bar(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let (crow, ccol) = self.cursor_cell().unwrap_or((0, 0));
        let cx_left = px(GRID_PAD_X) + self.cell_width * (ccol as f32);
        let shift = self.input_scroll_rows();
        let cy_top = px(GRID_PAD_Y) + self.line_height * (crow as f32 - shift as f32);

        if let Some(rs) = &self.reverse_search {
            let label = format!("(reverse-i-search)`{}': ", rs.query());
            let matched = one_line(rs.selected_line(&self.history).unwrap_or_default());
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
        let mut sel_bg = theme.selection;
        sel_bg.a = 0.55;
        let cell_w = self.cell_width;
        let lh = self.line_height;
        let caret_h = px((self.font_size.as_f32() * 1.2).min(lh.as_f32()));
        let caret_top = px((lh.as_f32() - caret_h.as_f32()) / 2.0);

        let line: String = chars.iter().collect();
        let mut colors: Vec<gpui::Hsla> = Vec::with_capacity(len);
        for span in highlight::highlight(&line) {
            let c = self.kind_color(span.kind, cx);
            for _ in span.text.chars() {
                colors.push(c);
            }
        }

        let cursor_on = self.cursor_visible;
        let cursor_style = cx.global::<Config>().cursor_style;
        let block_cursor = cursor_style == crate::core::config::CursorStyle::Block;
        // A block caret is drawn as reverse video on the cell it covers, not as
        // a translucent tint over it — the way every other terminal draws one,
        // and the only way the caret keeps the contrast the theme gave it.
        let caret_ink = crate::ui::presets::caret_ink(caret_col, theme.background, fg);
        let caret_bar = move || {
            use crate::core::config::CursorStyle;
            let base = div().absolute().left_0().bg(caret_col);
            match cursor_style {
                CursorStyle::Bar => base.top(caret_top).w(px(1.5)).h(caret_h),
                CursorStyle::Block => base.top(px(0.)).w_full().h(lh),
                CursorStyle::Underline => {
                    let uh = px(2.);
                    base.top(lh - uh).w_full().h(uh)
                }
            }
        };
        let cell = |color: gpui::Hsla, ch: char, selected: bool, caret: bool, underline: bool| {
            let inverted = caret && block_cursor;
            let w = cell_w * (display_width(ch) as f32);
            let mut d = div()
                .relative()
                .flex_none()
                .w(w)
                .h(lh)
                .flex()
                .items_center()
                .text_color(if inverted { caret_ink } else { color });
            if inverted {
                d = d.bg(caret_col);
            } else if selected {
                d = d.bg(sel_bg);
            }
            if underline {
                d = d.border_b_1().border_color(fg);
            }
            d = d.child(ch.to_string());
            if caret && !inverted {
                d = d.child(caret_bar());
            }
            d.into_any_element()
        };

        let blank = move |w: gpui::Pixels| div().flex_none().w(w).h(lh);

        let mut lines: Vec<Vec<gpui::AnyElement>> =
            vec![vec![blank(cell_w * (ccol as f32)).into_any_element()]];

        let is_multiline = chars.contains(&'\n');

        for i in 0..len {
            if i == cursor && has_marked {
                for mc in marked.chars() {
                    lines
                        .last_mut()
                        .unwrap()
                        .push(cell(fg, mc, false, false, true));
                }
            }
            if chars[i] == '\n' {
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

        let ghost: Option<String> = if selection.is_none() && !has_marked && !is_multiline {
            self.ghost_suggestion()
                .map(|full| full.chars().skip(len).collect::<String>())
                .filter(|r| !r.is_empty())
        } else {
            None
        };

        if cursor == len {
            let last = lines.last_mut().unwrap();
            if has_marked {
                for mc in marked.chars() {
                    last.push(cell(fg, mc, false, false, true));
                }
            } else if ghost.is_none() {
                let mut tail = blank(cell_w).relative();
                if selection.is_none() && cursor_on {
                    tail = tail.child(caret_bar());
                }
                last.push(tail.into_any_element());
            }
        }

        if let Some(rem) = ghost {
            let last = lines.last_mut().unwrap();
            for (gi, gc) in rem.chars().map(one_line_char).enumerate() {
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
            .font_family(self.font.family.clone())
            .text_size(self.font_size)
            .line_height(lh)
            .text_color(fg)
            .children(rows)
    }

    fn render_completion_menu(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let s = self.completion.as_ref()?;
        let items: Vec<&completion::Candidate> = s.filtered.iter().map(|&i| &s.all[i]).collect();
        if items.is_empty() {
            return None;
        }
        let (srow, scol) = self.cursor_cell()?;
        let srow = srow.saturating_sub(self.input_scroll_rows());

        const MAX_ROWS: usize = 10;
        let (total_rows, total_cols) = {
            let term = self.terminal.term.lock();
            (term.screen_lines(), term.columns())
        };
        // How wide the menu ends up, decided before the rows so their
        // descriptions can be elided against it. A menu wider than the pane
        // has its right-hand column clipped by the pane, and the clip takes
        // the ellipsis with it — which is exactly the mid-word cut the
        // ellipsis exists to prevent. The history menu below already caps
        // itself to the grid this way.
        let grid_w = self.cell_width * (total_cols as f32);
        let menu_w = px(COMPLETION_MENU_MAX_W).min(grid_w);
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
        let lh = self.line_height;
        let cell = self.cell_width.as_f32();
        let row = |i: usize| {
            let cand = items[i];
            let selected = s.index == Some(i);
            let icon_color = if selected {
                theme.foreground
            } else {
                theme.muted_foreground
            };
            let icon = completion_row_icon(cand.icon.as_deref(), cand.kind, icon_color);
            let label = if cand.is_dir() && !cand.text.ends_with('/') {
                format!("{}/", cand.text)
            } else {
                cand.text.clone()
            };
            let budget = description_budget(cell, label.chars().count(), menu_w.as_f32());
            div()
                .h(lh)
                .flex()
                .items_center()
                .gap_1p5()
                .px_2()
                .whitespace_nowrap()
                .when(selected, |d| {
                    d.bg(theme.list_active).text_color(theme.foreground)
                })
                .child(icon)
                .child(div().flex_shrink_0().child(label))
                .when_some(cand.description.clone(), |d, desc| {
                    d.child(
                        div()
                            .ml_2()
                            .text_color(theme.muted_foreground)
                            .child(elide(&desc, budget)),
                    )
                })
                .into_any_element()
        };
        let rows: Vec<gpui::AnyElement> = (first..first + visible).map(row).collect();

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

        let gap = px(6.);
        // Anchored under the word being completed, then pulled back inside the
        // pane if that would hang it over the right edge.
        let anchor = px(GRID_PAD_X) + self.cell_width * (scol as f32);
        let x = anchor
            .min(px(GRID_PAD_X) + grid_w - menu_w)
            .max(px(GRID_PAD_X));
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
                .min_w(px(120.).min(menu_w))
                .max_w(menu_w)
                .overflow_hidden()
                .bg(theme.popover)
                .border_1()
                .border_color(theme.border)
                .rounded(px(6.))
                .font_family(self.font.family.clone())
                .text_size(self.font_size)
                // Resting rows sit back so the selected row, which paints
                // itself in the full foreground, is the brightest line in
                // the menu — matching what the row icons already do.
                .text_color(theme.muted_foreground)
                .children(footer(hidden_above, format!("↑ {hidden_above} more")))
                .children(rows)
                .children(footer(hidden_below, format!("↓ {hidden_below} more"))),
        )
    }

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
                theme.muted_foreground
            };

            let spans: Vec<gpui::AnyElement> = highlight_runs(line, &m.positions)
                .into_iter()
                .map(|(run, hit)| {
                    div()
                        .flex_none()
                        .whitespace_nowrap()
                        .text_color(if hit { theme.blue } else { base })
                        .child(run)
                        .into_any_element()
                })
                .collect();

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
                .text_color(theme.muted_foreground)
                .children(footer(hidden_above, format!("↑ {hidden_above} more")))
                .children(rows)
                .children(footer(hidden_below, format!("↓ {hidden_below} more"))),
        )
    }

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

fn is_typeahead_interrupt(key: &str, modifiers: &Modifiers) -> bool {
    modifiers.control && !modifiers.alt && !modifiers.platform && key == "c"
}

fn sync_typeahead_owner_state(
    typeahead: &mut Typeahead,
    last_blocked: &mut bool,
    blocked: bool,
) -> bool {
    // Alternate-screen TUIs and known agents own their input. Never replay a
    // record across an ownership boundary as shell input.
    let changed = blocked != *last_blocked;
    if changed {
        typeahead.discard();
        *last_blocked = blocked;
    }
    changed
}

fn observe_typeahead_for_owner(
    typeahead: &mut Typeahead,
    last_blocked: &mut bool,
    input: RawInput<'_>,
    blocked: bool,
) {
    if !sync_typeahead_owner_state(typeahead, last_blocked, blocked) {
        typeahead.observe(input, *last_blocked);
    }
}

/// Why closing a pane would end work that is still going on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaneBusy {
    Command(String),
    Agent(&'static str),
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Drop for TerminalView {
    fn drop(&mut self) {
        self.flush_pending_history();
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_typeahead_owner();
        self.sync_scrollbar();
        if self.shell_owns_prompt() {
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

        let input_bar = self.input_active().then(|| self.render_input_bar(cx));
        let completion_menu = self
            .input_active()
            .then(|| self.render_completion_menu(cx))
            .flatten();
        let reverse_search_menu = self
            .input_active()
            .then(|| self.render_reverse_search_menu(cx))
            .flatten();
        let integration_notice = self.render_integration_notice(cx);

        let menu_focus = self.focus_handle.clone();
        let has_selection = self.any_selection();
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
            .text_color(cx.theme().foreground)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev: &MouseDownEvent, window, cx| {
                    if window.default_prevented() {
                        return;
                    }
                    window.focus(&this.focus_handle, cx);
                }),
            )
            // Latch the context-menu verdict for this click. gpui-component's
            // `ContextMenu` element owns the right mouse-down that opens the
            // popup and hands the builder no event to inspect, so the modifiers
            // have to be recorded here. That element wraps this one, so its
            // listener actually fires *before* this one — harmless, because it
            // only builds the menu from a `window.defer` callback that runs
            // once the whole mouse dispatch has unwound.
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, ev: &MouseDownEvent, _window, _cx| {
                    this.context_menu_allowed =
                        should_show_context_menu(this.mouse_mode(), ev.modifiers.shift);
                }),
            )
            .drag_over::<ExternalPaths>(|s, _, _, cx| s.bg(cx.theme().drag_border.opacity(0.12)))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                window.focus(&this.focus_handle, cx);
                this.drop_files(paths, cx);
            }))
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
            .on_action(cx.listener(|this, _: &FindNext, _w, cx| {
                this.step_match(Direction::Right, cx);
            }))
            .on_action(cx.listener(|this, _: &FindPrevious, _w, cx| {
                this.step_match(Direction::Left, cx);
            }))
            .on_action(cx.listener(|this, _: &ClearScrollback, _w, cx| this.clear_scrollback(cx)))
            .on_action(cx.listener(|this, _: &InsertNewline, _w, cx| {
                this.insert_newline_action(cx);
            }))
            .on_action(cx.listener(|this, _: &InsertNewlineFallback, _w, cx| {
                this.insert_newline_fallback_action(cx);
            }))
            .on_action(cx.listener(|this, _: &SendTab, _w, cx| {
                this.tab_pressed(true, cx);
            }))
            .on_action(cx.listener(|this, _: &SendBackTab, _w, cx| {
                this.tab_pressed(false, cx);
            }))
            .child(TerminalElement::new(entity))
            .child(self.render_scrollbar())
            .children(search_bar)
            .children(input_bar)
            .children(completion_menu)
            .children(reverse_search_menu)
            .children(integration_notice)
            .context_menu(move |menu, window, cx| {
                // Suppressing the popup means handing back an item-less menu:
                // gpui-component's `ContextMenu` element skips rendering the
                // anchored overlay entirely when the built menu `is_empty()`,
                // and its right mouse-down listener is registered after ours
                // (it wraps this element), so we cannot out-order it or veto it
                // any earlier. See `should_show_context_menu` for the rule.
                if !menu_view.read(cx).context_menu_allowed {
                    return menu;
                }
                let menu = menu
                    .min_w(px(220.))
                    .action_context(menu_focus.clone())
                    .menu_element_with_disabled(
                        Box::new(CopyText),
                        !has_selection,
                        menu_row_with_hint(t(L10nKey::AppMenuCopy), Some("secondary-c")),
                    )
                    .menu_element_with_disabled(
                        Box::new(CutText),
                        !has_selection,
                        menu_row_with_hint(t(L10nKey::AppMenuCut), Some("secondary-x")),
                    )
                    .menu_element(
                        Box::new(PasteText),
                        menu_row_with_hint(t(L10nKey::AppMenuPaste), Some("secondary-v")),
                    )
                    .menu_element(
                        Box::new(SelectAll),
                        menu_row_with_hint(t(L10nKey::AppMenuSelectAll), mac_only("secondary-a")),
                    )
                    .separator()
                    .menu(t(L10nKey::AppMenuFind), Box::new(FindInTerminal))
                    .menu(
                        t(L10nKey::AppMenuClearScrollback),
                        Box::new(ClearScrollback),
                    );

                let view = menu_view.read(cx);
                // `fork_label` is tty7-core's capability probe, and core has no
                // locale table — take the answer, not its English wording.
                let can_fork = view.agent().and_then(|a| a.fork_label()).is_some();
                let fork_ready = can_fork
                    && view.remote_context().is_none()
                    && view.agent_session().is_some_and(|s| s.session_id.is_some());

                let menu = match (can_fork, fork_ready) {
                    (true, true) => {
                        let focus = menu_focus.clone();
                        menu.separator().submenu(
                            t(L10nKey::AppMenuForkSession),
                            window,
                            cx,
                            move |submenu, _window, _cx| {
                                submenu
                                    .action_context(focus.clone())
                                    .menu(
                                        t(L10nKey::AppMenuSplitRight),
                                        Box::new(ForkAgentSessionRight),
                                    )
                                    .menu(
                                        t(L10nKey::AppMenuSplitLeft),
                                        Box::new(ForkAgentSessionLeft),
                                    )
                                    .menu(
                                        t(L10nKey::AppMenuSplitDown),
                                        Box::new(ForkAgentSessionDown),
                                    )
                                    .menu(t(L10nKey::AppMenuSplitUp), Box::new(ForkAgentSessionUp))
                            },
                        )
                    }
                    (true, false) => menu
                        .separator()
                        .item(PopupMenuItem::new(t(L10nKey::AppMenuForkSession)).disabled(true)),
                    (false, _) => menu,
                };

                menu.separator()
                    .menu(t(L10nKey::AppMenuSplitRight), Box::new(SplitRight))
                    .menu(t(L10nKey::AppMenuSplitDown), Box::new(SplitDown))
                    .menu(t(L10nKey::AppMenuZoomPane), Box::new(ToggleMaximizePane))
                    .separator()
                    .menu(t(L10nKey::AppMenuNewTab), Box::new(NewTab))
                    .menu(t(L10nKey::AppMenuClosePaneTab), Box::new(CloseActiveTab))
            })
    }
}

fn menu_row_with_hint(
    label: &'static str,
    key: Option<&'static str>,
) -> impl Fn(&mut Window, &mut App) -> gpui::AnyElement {
    move |_window, _cx| {
        let hint = key.map(|k| {
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

#[cfg(target_os = "macos")]
fn mac_only(key: &'static str) -> Option<&'static str> {
    Some(key)
}
#[cfg(not(target_os = "macos"))]
fn mac_only(_key: &'static str) -> Option<&'static str> {
    None
}

fn word_start_of(line: &str, cursor: usize) -> usize {
    let chars: Vec<char> = line.chars().collect();
    let mut start = cursor.min(chars.len());
    while start > 0 && !chars[start - 1].is_whitespace() {
        start -= 1;
    }
    start
}

/// A history entry drawn on a one-line row, with its line breaks folded to a
/// visible stand-in.
///
/// gpui breaks text on `\n` whatever `white_space` says, so an entry carrying
/// one paints its tail over whatever sits below it. Entries do carry them: a
/// command composed in the inline editor keeps its newlines in the in-session
/// list, even though `history::append` refuses to write them to disk. Folding
/// is one char for one char so a fuzzy matcher's positions still address the
/// same characters.
fn one_line_char(c: char) -> char {
    match c {
        '\n' => '↵',
        c if c.is_control() => ' ',
        c => c,
    }
}

fn one_line(text: &str) -> String {
    text.chars().map(one_line_char).collect()
}

/// Split `line` into consecutive runs of matched / unmatched characters, each
/// one folded onto a single line by `one_line_char`. `positions` is ascending.
fn highlight_runs(line: &str, positions: &[usize]) -> Vec<(String, bool)> {
    let mut runs: Vec<(String, bool)> = Vec::new();
    let mut pos = positions.iter().copied().peekable();
    for (ci, ch) in line.chars().enumerate() {
        let hit = pos.next_if_eq(&ci).is_some();
        let ch = one_line_char(ch);
        match runs.last_mut() {
            Some((run, run_hit)) if *run_hit == hit => run.push(ch),
            _ => runs.push((ch.to_string(), hit)),
        }
    }
    runs
}

fn display_width(c: char) -> usize {
    let u = c as u32;
    let wide = matches!(u,
        0x1100..=0x115F
        | 0x2329 | 0x232A
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE10..=0xFE19 | 0xFE30..=0xFE6F
        | 0xFF00..=0xFF60 | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1FAFF
        | 0x20000..=0x3FFFD
    );
    if wide { 2 } else { 1 }
}

#[derive(Debug, PartialEq)]
enum WheelRoute {
    Report { base: u8 },
    Arrows { seq: &'static [u8] },
    Scrollback,
}

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

#[derive(Debug, PartialEq)]
enum SelectEndCopy {
    None,
    Grid,
    Editor,
}

/// Whether a right-click belongs to tty7's own context menu rather than to the
/// terminal application.
///
/// A TUI that has turned mouse reporting on (vim with `set mouse=a`, lazygit,
/// tmux, …) draws its own right-button menus, so popping the host menu on top
/// of the application's is a double delivery of one click — #251. While
/// reporting is active the unmodified click is the application's alone; Shift
/// is the escape hatch that reaches tty7, matching how Shift already overrides
/// mouse reporting for selection (`register_mouse_handlers`) and for the wheel
/// (`wheel_route`).
///
/// This is the exact complement of the "forward the press to the app" branch in
/// `TerminalElement::register_mouse_handlers`, which is why both call it: one
/// click must never feed both consumers.
pub(super) fn should_show_context_menu(mouse_mode: bool, shift: bool) -> bool {
    !mouse_mode || shift
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
            break;
        };
        let close = open + close_rel;
        out.push_str(&rest[..open]);
        let value = match &rest[open + 1..close] {
            "path" => Some(path.to_string()),
            "line" => line.map(|l| l.to_string()),
            "column" => column.map(|c| c.to_string()),
            other => Some(format!("{{{other}}}")),
        };
        out.push_str(&value?);
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    Some(out)
}

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

fn focus_report_bytes(mode: TermMode, focused: bool) -> Option<&'static [u8]> {
    if !mode.contains(TermMode::FOCUS_IN_OUT) {
        return None;
    }
    Some(if focused { b"\x1b[I" } else { b"\x1b[O" })
}

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

fn fig_query_param<'a>(raw: &'a str, key: &str) -> Option<&'a str> {
    raw.split_once('?')?.1.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then_some(v)
    })
}

/// Where the completion menu stops growing.
const COMPLETION_MENU_MAX_W: f32 = 480.;

/// How many characters of a candidate's description fit beside its name.
///
/// The menu is monospaced and clips whatever runs past its edge, so a long
/// description used to end mid-word — "…or a symli" — with nothing to say it
/// had been cut. The row spends its width on `px_2` either side, the kind
/// icon, the gap after it and the margin before the description; whatever is
/// left, in cells, is the description's.
///
/// `menu_w` is the width the menu actually got, not the width it would like:
/// in a pane narrower than [`COMPLETION_MENU_MAX_W`] the menu is capped to the
/// grid, and a budget measured against the larger number puts the ellipsis
/// past the edge — which is the same mid-word cut, only harder to see.
fn description_budget(cell_width: f32, label_cells: usize, menu_w: f32) -> usize {
    const ROW_CHROME: f32 = 8. + 8. + 16. + 6. + 8.;
    if cell_width <= 0. {
        return 0;
    }
    let free = menu_w - ROW_CHROME - label_cells as f32 * cell_width;
    (free / cell_width).floor().max(0.) as usize
}

/// `text` cut to `budget` characters, with the last one spent on an ellipsis.
/// A budget too small to say anything with returns nothing rather than a bare
/// "…", which reads as a description that is there but unreadable.
fn elide(text: &str, budget: usize) -> String {
    if text.chars().count() <= budget {
        return text.to_string();
    }
    if budget < 2 {
        return String::new();
    }
    let mut out: String = text.chars().take(budget - 1).collect();
    out.push('…');
    out
}

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
    let footers = if count > want { 2 } else { 0 };
    let need = want + footers;
    let (place_above, visible) = if below >= need {
        (false, want)
    } else if above >= need {
        (true, want)
    } else {
        let squeeze = |room: usize| room.saturating_sub(2).max(1);
        if above > below {
            (true, squeeze(above))
        } else {
            (false, squeeze(below))
        }
    };
    let visible = visible.min(count);
    let first = sel
        .saturating_sub(visible.saturating_sub(1))
        .min(count.saturating_sub(visible));
    (place_above, visible, first)
}

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
    let (positions, r, c) = input_char_positions(chars, scol, cols);
    let end_row = if c >= cols { r + 1 } else { r };
    if target > end_row {
        return clamp.then_some(len);
    }
    for (i, &(pr, pc, pw)) in positions.iter().enumerate() {
        if pr == target && col >= pc && col < pc + pw {
            return Some(i);
        }
    }
    if let Some(fi) = positions.iter().position(|&(pr, _, _)| pr == target) {
        if col < positions[fi].1 {
            return Some(fi);
        }
    }
    if let Some(last) = positions.iter().rposition(|&(pr, _, _)| pr == target) {
        if chars[last] == '\n' {
            return Some(last);
        }
    }
    match positions.iter().position(|&(pr, _, _)| pr > target) {
        Some(ni) => Some(ni),
        None => Some(len),
    }
}

/// How many font-size steps a zoom event is worth, and what is left over for
/// the next one.
///
/// A wheel detent is a discrete click of intent, so it is one step whatever the
/// platform says it covers — macOS calls a single notch five lines, and five
/// points of font per notch would be unusable. A trackpad has no detents and
/// spends a flick over dozens of events, so those accumulate and only pay out
/// once the fingers have travelled [`ZOOM_SCROLL_LINES`].
fn zoom_scroll_steps(lines: f32, debt: f32, gesturing: bool) -> (i32, f32) {
    if !gesturing {
        let step = if lines > 0. {
            1
        } else if lines < 0. {
            -1
        } else {
            0
        };
        return (step, 0.);
    }
    let total = debt + lines;
    let steps = (total / ZOOM_SCROLL_LINES).trunc();
    (steps as i32, total - steps * ZOOM_SCROLL_LINES)
}

fn smooth_scroll_step(offset: usize, frac: f32, delta: f32, max: usize) -> (i32, f32) {
    let pos = (offset as f32 + frac + delta).clamp(0., max as f32);
    let new_offset = pos.floor();
    (new_offset as i32 - offset as i32, pos - new_offset)
}

/// Advance the in-flight scroll animation once per presented frame.
///
/// Registered from [`TerminalView::queue_scroll_anim`]: gpui runs the callback
/// immediately before the next frame is drawn, and it re-registers itself as
/// long as the animation lives. Stepping at frame time instead of on a
/// free-running timer keeps the decay steps locked to the monitor's vblank
/// cadence — a 16 ms timer drifting against a 16.67 ms vblank makes some
/// presented frames show twice the movement of their neighbours and others
/// none, which reads as stutter. Frame-aligned stepping shows every frame
/// exactly the ground it covered. The weak handle and the epoch guard make the
/// chain die quietly when the pane is closed or the animation is cancelled.
fn schedule_scroll_anim_frame(view: WeakEntity<TerminalView>, epoch: u64, window: &mut Window) {
    window.on_next_frame(move |window, cx| {
        let Some(view) = view.upgrade() else {
            return;
        };
        if view.update(cx, |view, cx| view.scroll_anim_frame(epoch, cx)) {
            schedule_scroll_anim_frame(view.downgrade(), epoch, window);
        }
    });
}

/// How much of `remaining` to consume this frame, and whether this is the last
/// one. Decay is scaled by the real elapsed time so a dropped frame covers the
/// ground it missed instead of stretching the animation out.
fn scroll_anim_step(remaining: f32, dt: std::time::Duration) -> (f32, bool) {
    if remaining.abs() <= SCROLL_ANIM_MIN {
        return (remaining, true);
    }
    let frames = (dt.as_secs_f32() / SCROLL_ANIM_FRAME.as_secs_f32()).clamp(0.1, 8.);
    let consumed = 1. - (1. - SCROLL_ANIM_SMOOTH).powf(frames);
    let step = remaining * consumed;
    if (remaining - step).abs() <= SCROLL_ANIM_MIN {
        (remaining, true)
    } else {
        (step, false)
    }
}

fn drag_scroll_step(overshoot: f32) -> i32 {
    let lines = overshoot.abs().ceil().clamp(1., 8.) as i32;
    if overshoot < 0. { -lines } else { lines }
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_busy_command_name_undoes_the_shell_escaping() {
        use super::unescape_mark_text;
        // The integration escapes % and the four bytes that would break OSC
        // framing; everything else is the line as typed.
        assert_eq!(unescape_mark_text("printf '100%25'"), "printf '100%'");
        assert_eq!(unescape_mark_text("a%1Bb%07c%0Dd"), "abcd");
        assert_eq!(unescape_mark_text("one%0Atwo"), "one two");
        // A bare % the shell somehow left alone must survive rather than eat
        // the next two characters.
        assert_eq!(unescape_mark_text("50% off"), "50% off");
        assert_eq!(unescape_mark_text("cargo build"), "cargo build");
    }

    #[test]
    fn a_busy_command_name_stays_short_enough_to_read() {
        use super::{BUSY_COMMAND_MAX, clamp_command};
        assert_eq!(clamp_command("  sleep 300  "), "sleep 300");
        let long = "cargo ".repeat(40);
        let out = clamp_command(&long);
        assert!(out.ends_with('…'), "{out:?}");
        assert!(out.chars().count() <= BUSY_COMMAND_MAX + 1, "{out:?}");
        // No dangling space before the ellipsis.
        assert!(!out.contains(" …"), "{out:?}");
    }
    use super::{
        COMPLETION_MENU_MAX_W, LoopbackPlan, RawInput, SelectEndCopy, Typeahead, WheelRoute,
        clipboard_paste_text, compose_notification_title, cwd_is_on_host, display_width,
        is_typeahead_interrupt, loopback_plan, observe_typeahead_for_owner,
    };
    use super::{SCROLL_ANIM_FRAME, scroll_anim_step};
    use super::{
        description_budget, drag_scroll_step, elide, encode_mouse, escape_candidate,
        expand_file_command_template, fallback_chain, fig_icon_emoji, fig_icon_glyph,
        focus_report_bytes, highlight_runs, input_overflow_shift, input_overlay_rows, menu_layout,
        paste_bytes, select_end_copy, shell_escape_path, should_show_context_menu,
        smooth_scroll_step, submit_bytes, trim_trailing_spaces, wheel_route, wrapped_click_index,
    };
    use super::{
        remote_paste_spec, staged_path_for_pane, stages_clipboard_image, staging_cache,
        staging_dir_is_safe, wsl_path, wsl_share_distro, wsl_share_path,
    };
    use alacritty_terminal::term::TermMode;
    use gpui::{ClipboardEntry, ClipboardItem, ExternalPaths, Modifiers};
    use gpui_component::IconName;
    use std::path::{Path, PathBuf};

    use crate::core::session::{RemoteTarget, WorkspaceId};
    use crate::daemon::protocol::RemoteKind;
    use crate::terminal::PaneWorkspace;

    #[test]
    fn a_notification_title_keeps_at_most_two_segments() {
        let ws = || Some("tty7".to_string());
        assert_eq!(
            compose_notification_title(None, Some("build-box".into()), ws()),
            "build-box · tty7"
        );
        // An agent takes the machine's place rather than adding a third part.
        assert_eq!(
            compose_notification_title(Some("Claude".into()), Some("build-box".into()), ws()),
            "Claude · tty7"
        );
        // A local pane has no machine label; a nameless workspace has no name.
        assert_eq!(compose_notification_title(None, None, ws()), "tty7");
        assert_eq!(
            compose_notification_title(Some("Claude".into()), None, None),
            "Claude"
        );
        assert_eq!(compose_notification_title(None, None, None), "tty7");
    }

    #[test]
    fn alt_screen_exit_discards_the_boundary_input_before_recording_shell_text() {
        let mut typeahead = Typeahead::new();
        let mut last_blocked = true;
        typeahead.observe(RawInput::Text("stale"), false);

        observe_typeahead_for_owner(
            &mut typeahead,
            &mut last_blocked,
            RawInput::Key {
                key: "c",
                plain: false,
            },
            false,
        );
        assert_eq!(
            typeahead.drain(),
            None,
            "the Ctrl-C crossing TUI exit must not become a tainted shell record"
        );

        observe_typeahead_for_owner(
            &mut typeahead,
            &mut last_blocked,
            RawInput::Text("ls"),
            false,
        );
        assert_eq!(typeahead.drain(), Some("ls".to_string()));
    }

    #[test]
    fn agent_interrupt_discards_typeahead_without_an_alt_screen_transition() {
        let mut typeahead = Typeahead::new();
        let mut last_blocked = false;
        typeahead.observe(RawInput::Text("agent input"), false);
        typeahead.observe(
            RawInput::Key {
                key: "up",
                plain: true,
            },
            false,
        );

        observe_typeahead_for_owner(
            &mut typeahead,
            &mut last_blocked,
            RawInput::Interrupt,
            false,
        );
        assert_eq!(
            typeahead.drain(),
            None,
            "Ctrl-C must cancel a stable non-ALT_SCREEN agent gap"
        );

        observe_typeahead_for_owner(
            &mut typeahead,
            &mut last_blocked,
            RawInput::Text("ls"),
            false,
        );
        assert_eq!(typeahead.drain(), Some("ls".to_string()));
    }

    #[test]
    fn known_agent_input_is_discarded_at_both_ownership_boundaries() {
        let mut typeahead = Typeahead::new();
        let mut last_blocked = false;
        typeahead.observe(RawInput::Text("stale shell gap"), false);

        observe_typeahead_for_owner(
            &mut typeahead,
            &mut last_blocked,
            RawInput::Text("agent input"),
            true,
        );
        observe_typeahead_for_owner(
            &mut typeahead,
            &mut last_blocked,
            RawInput::Key {
                key: "up",
                plain: true,
            },
            true,
        );
        assert_eq!(typeahead.drain(), None);

        observe_typeahead_for_owner(
            &mut typeahead,
            &mut last_blocked,
            RawInput::Text("boundary input"),
            false,
        );
        assert_eq!(typeahead.drain(), None);

        observe_typeahead_for_owner(
            &mut typeahead,
            &mut last_blocked,
            RawInput::Text("ls"),
            false,
        );
        assert_eq!(typeahead.drain(), Some("ls".to_string()));
    }

    #[test]
    fn only_plain_ctrl_c_is_a_typeahead_interrupt() {
        let ctrl = Modifiers {
            control: true,
            ..Default::default()
        };
        assert!(is_typeahead_interrupt("c", &ctrl));
        assert!(!is_typeahead_interrupt("d", &ctrl));

        let ctrl_alt = Modifiers {
            control: true,
            alt: true,
            ..Default::default()
        };
        assert!(!is_typeahead_interrupt("c", &ctrl_alt));
    }

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

    #[test]
    fn local_pane_opens_localhost_directly() {
        assert_eq!(loopback_plan(true, None, None, 1), LoopbackPlan::Direct);
    }

    #[test]
    fn ssh_pane_forwards_on_the_pane() {
        assert_eq!(
            loopback_plan(true, None, Some(RemoteKind::NativeSsh), 7),
            LoopbackPlan::ForwardOnPane(7)
        );
        assert_eq!(
            loopback_plan(true, None, Some(RemoteKind::Wsl), 7),
            LoopbackPlan::Direct
        );
    }

    #[test]
    fn remote_workspace_pane_forwards_on_the_workspace() {
        let w = ws(RemoteTarget::direct("me", "dev.box", 22), true);
        assert_eq!(
            loopback_plan(true, Some(&w), None, 7),
            LoopbackPlan::ForwardOnWorkspace(Box::new(w.clone())),
            "no RemoteContext, but still forwarded"
        );
        assert_eq!(
            loopback_plan(true, Some(&w), Some(RemoteKind::NativeSsh), 7),
            LoopbackPlan::ForwardOnWorkspace(Box::new(w))
        );
    }

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

    #[test]
    fn workspace_without_a_spec_does_not_forward() {
        let w = ws(RemoteTarget::direct("me", "dev.box", 22), false);
        assert_eq!(loopback_plan(true, Some(&w), None, 7), LoopbackPlan::Direct);
    }

    /// The SSH user a paste would be uploaded for, or `None` when the pane
    /// keeps the local-path behavior.
    fn remote_paste_user<'a>(
        workspace: Option<&'a crate::terminal::PaneWorkspace>,
        ssh_spec: Option<&'a crate::daemon::protocol::NativeSshSpec>,
    ) -> Option<&'a str> {
        remote_paste_spec(workspace, ssh_spec).map(|s| s.user.as_str())
    }

    fn native_spec() -> crate::daemon::protocol::NativeSshSpec {
        serde_json::from_str(r#"{"host":"dev.box","port":22,"user":"me","auth_mode":"auto"}"#)
            .unwrap()
    }

    #[test]
    fn local_pane_pastes_the_local_image_path() {
        assert_eq!(remote_paste_user(None, None), None);
    }

    #[test]
    fn ssh_workspace_panes_upload_images_for_the_ssh_user() {
        let w = ws(RemoteTarget::direct("me", "dev.box", 22), true);
        assert_eq!(remote_paste_user(Some(&w), None), Some("me"));
    }

    #[test]
    fn standalone_ssh_panes_upload_images_for_the_ssh_user() {
        let spec = native_spec();
        assert_eq!(remote_paste_user(None, Some(&spec)), Some("me"));
    }

    fn wsl_context(distro: &str) -> crate::daemon::protocol::RemoteContext {
        crate::daemon::protocol::RemoteContext {
            kind: RemoteKind::Wsl,
            argv: Vec::new(),
            target: distro.to_string(),
        }
    }

    #[test]
    fn a_wsl_exe_pane_on_this_machine_completes_over_its_own_share() {
        assert_eq!(
            wsl_share_distro(Some(&wsl_context("Ubuntu-24.04")), None, true),
            Some("Ubuntu-24.04".to_string())
        );
    }

    /// The same pane on a remote machine reaches that machine's distro, which
    /// no share here can list.
    #[test]
    fn a_remote_hosts_wsl_exe_pane_leaves_tab_to_the_shell() {
        assert_eq!(
            wsl_share_distro(Some(&wsl_context("Ubuntu-24.04")), None, false),
            None
        );
    }

    /// A WSL workspace's panes are served by the daemon inside the distro, so
    /// their host is never `LOCAL` — and the distro is still on this machine.
    #[test]
    fn a_wsl_workspace_pane_completes_over_the_share_its_target_names() {
        let w = ws(
            RemoteTarget::Wsl {
                distro: "Ubuntu-24.04".into(),
            },
            false,
        );
        assert_eq!(
            wsl_share_distro(None, Some(&w), false),
            Some("Ubuntu-24.04".to_string())
        );
    }

    #[test]
    fn panes_with_no_distro_of_their_own_have_no_share() {
        let ssh = ws(RemoteTarget::direct("me", "dev.box", 22), true);
        assert_eq!(wsl_share_distro(None, Some(&ssh), false), None);
        assert_eq!(wsl_share_distro(None, None, true), None);
    }

    #[test]
    fn wsl_and_specless_workspaces_keep_the_local_image_path() {
        let wsl = ws(
            RemoteTarget::Wsl {
                distro: "Ubuntu".into(),
            },
            false,
        );
        assert_eq!(remote_paste_user(Some(&wsl), None), None);
        let bare = ws(RemoteTarget::direct("me", "dev.box", 22), false);
        assert_eq!(remote_paste_user(Some(&bare), None), None);
    }

    #[test]
    fn a_staging_dir_is_only_safe_when_it_is_a_private_directory_we_own() {
        use crate::daemon::protocol::SftpEntryKind;
        // `chmod 0700` succeeded and the mode came back as asked: ours.
        assert!(staging_dir_is_safe(
            false,
            Some(SftpEntryKind::Dir),
            0o040700
        ));
        // A mode anyone else can enter is one anyone else can read pastes from.
        assert!(!staging_dir_is_safe(
            false,
            Some(SftpEntryKind::Dir),
            0o040755
        ));
        assert!(!staging_dir_is_safe(
            false,
            Some(SftpEntryKind::Dir),
            0o040701
        ));
        // Sticky/setgid bits mean someone else set the terms.
        assert!(!staging_dir_is_safe(
            false,
            Some(SftpEntryKind::Dir),
            0o041700
        ));
        // A symlink is judged by its target by `stat`, so refuse it outright.
        assert!(!staging_dir_is_safe(
            true,
            Some(SftpEntryKind::Dir),
            0o040700
        ));
        // A file (or a path that vanished) is not a staging dir.
        assert!(!staging_dir_is_safe(
            false,
            Some(SftpEntryKind::File),
            0o100700
        ));
        assert!(!staging_dir_is_safe(false, None, 0o040700));
    }

    #[test]
    fn a_remote_pane_stages_its_clipboard_image_on_every_platform() {
        // The SYN path leans on the agent sharing a clipboard with the pane,
        // which a remote agent never does — it reads the clipboard of the host
        // it runs on, and this machine's screenshot is not in it.
        assert!(
            stages_clipboard_image(true),
            "a remote agent cannot see this machine's clipboard"
        );
        // Locally the platform decides: macOS hands the agent the clipboard
        // itself, which is higher fidelity than a staged file.
        assert_eq!(
            stages_clipboard_image(false),
            cfg!(not(target_os = "macos"))
        );
    }

    #[test]
    fn a_wsl_pane_gets_the_automount_path_not_the_windows_one() {
        // The staged file really is on the pane's own disk — only its name
        // differs — so this is a rewrite, not an upload.
        assert_eq!(
            staged_path_for_pane(
                r"C:\Users\me\AppData\Local\Temp\tty7-clipboard\paste-1.png",
                true
            ),
            "/mnt/c/Users/me/AppData/Local/Temp/tty7-clipboard/paste-1.png"
        );
        assert_eq!(wsl_path(r"D:\x\y.png").as_deref(), Some("/mnt/d/x/y.png"));

        // No automount mapping: the Windows path at least says where it went.
        let unc = r"\\server\share\paste-1.png";
        assert_eq!(wsl_path(unc), None);
        assert_eq!(staged_path_for_pane(unc, true), unc);
        // Drive-relative, not absolute — `C:x` means "x under C:'s cwd".
        assert_eq!(wsl_path(r"C:paste-1.png"), None);

        // Every other pane keeps the path exactly as staged.
        assert_eq!(
            staged_path_for_pane("/tmp/tty7-clipboard/paste-1.png", false),
            "/tmp/tty7-clipboard/paste-1.png"
        );
        assert_eq!(
            staged_path_for_pane(r"C:\Temp\paste-1.png", false),
            r"C:\Temp\paste-1.png"
        );
    }

    #[test]
    fn a_wsl_cwd_gets_a_windows_spelling_the_completion_engine_can_list() {
        let share = |posix: &str| wsl_share_path("Ubuntu-24.04", posix);

        // A distro-native path goes through the share.
        assert_eq!(
            share("/home/me/repo"),
            Some(PathBuf::from(r"\\wsl$\Ubuntu-24.04\home\me\repo"))
        );
        assert_eq!(share("/"), Some(PathBuf::from(r"\\wsl$\Ubuntu-24.04\")));

        // The automount stays on the share too: a drive-spelled cwd would
        // send an absolute word (`ls /etc<Tab>`) to `C:\etc` instead of the
        // distro's /etc, because a rooted word completes against its cwd's
        // path prefix.
        assert_eq!(
            share("/mnt/c/Users/me"),
            Some(PathBuf::from(r"\\wsl$\Ubuntu-24.04\mnt\c\Users\me"))
        );

        // No absolute POSIX path, no translation — and a distro name that
        // could break out of the share is refused outright.
        assert_eq!(share("relative/path"), None);
        assert_eq!(wsl_share_path("", "/home/me"), None);
        assert_eq!(wsl_share_path(r"evil\distro", "/home/me"), None);
        assert_eq!(wsl_share_path("evil/distro", "/home/me"), None);
    }

    #[test]
    fn a_failed_staging_preparation_is_retried_rather_than_latched() {
        assert_eq!(
            staging_cache(&Ok("/home/me/.cache/tty7/clipboard".to_string())),
            Some("/home/me/.cache/tty7/clipboard".to_string())
        );
        // Nothing was created, so the next paste must try again instead of
        // handing out a path under a directory that does not exist.
        assert_eq!(staging_cache(&Err("link is down".to_string())), None);
    }

    #[test]
    fn staged_images_land_under_the_remote_users_own_home() {
        let mut dir = "/home/me".to_string();
        for component in super::REMOTE_CLIPBOARD_PATH {
            dir = crate::daemon::ssh::sftp::remote_join(&dir, component);
        }
        assert_eq!(dir, "/home/me/.cache/tty7/clipboard");
        assert!(
            !dir.starts_with("/tmp"),
            "a world-writable staging dir is exactly what this avoids"
        );
        assert_eq!(super::REMOTE_CLIPBOARD_MODE, 0o700);
    }

    #[test]
    fn the_pasted_image_name_stands_alone_as_a_remote_path_component() {
        use crate::daemon::ssh::sftp::safe_local_name;
        use gpui::{Image, ImageFormat};

        let pixel = image::RgbaImage::from_pixel(1, 1, image::Rgba([4, 5, 6, 255]));
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(pixel)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let path = super::write_clipboard_image(&Image::from_bytes(ImageFormat::Png, png)).unwrap();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(safe_local_name(&name), "{name} must not traverse or nest");
    }

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
        let argv = expand_file_command_template(
            "herdr edit {path} --line={line} --column={column}",
            Path::new("/tmp/foo.rs"),
            None,
            None,
        );
        assert_eq!(argv, vec!["herdr", "edit", "/tmp/foo.rs"]);

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
        let argv = expand_file_command_template(
            "code --goto {path}:{line} {other}",
            Path::new("/tmp/foo.rs"),
            None,
            None,
        );
        assert_eq!(argv, vec!["code", "--goto", "{other}"]);
    }

    #[test]
    fn clipboard_image_transcodes_bmp_to_png_and_passes_png_through() {
        use gpui::{Image, ImageFormat};

        let pixel = image::RgbaImage::from_pixel(1, 1, image::Rgba([1, 2, 3, 255]));
        let mut bmp = Vec::new();
        image::DynamicImage::ImageRgba8(pixel)
            .write_to(&mut std::io::Cursor::new(&mut bmp), image::ImageFormat::Bmp)
            .unwrap();
        let path = super::write_clipboard_image(&Image::from_bytes(ImageFormat::Bmp, bmp)).unwrap();
        assert_eq!(path.extension().unwrap(), "png");
        assert_eq!(&std::fs::read(&path).unwrap()[..8], b"\x89PNG\r\n\x1a\n");

        let png = std::fs::read(&path).unwrap();
        let out = super::write_clipboard_image(&Image::from_bytes(ImageFormat::Png, png.clone()))
            .unwrap();
        assert_eq!(out.extension().unwrap(), "png");
        assert_eq!(std::fs::read(&out).unwrap(), png);
    }

    /// A macOS screenshot reaches the pasteboard as TIFF, which agent vision
    /// rejects the same way it rejects a Windows BMP. The transcode depends on
    /// `image`'s TIFF decoder being compiled in — without it this fails at
    /// runtime by quietly staging nothing, which reads as "paste did nothing".
    #[test]
    fn a_macos_tiff_screenshot_is_staged_as_png() {
        use gpui::{Image, ImageFormat};

        let pixel = image::RgbaImage::from_pixel(1, 1, image::Rgba([4, 5, 6, 255]));
        let mut tiff = Vec::new();
        image::DynamicImage::ImageRgba8(pixel)
            .write_to(
                &mut std::io::Cursor::new(&mut tiff),
                image::ImageFormat::Tiff,
            )
            .unwrap();
        let path =
            super::write_clipboard_image(&Image::from_bytes(ImageFormat::Tiff, tiff)).unwrap();
        assert_eq!(path.extension().unwrap(), "png");
        assert_eq!(&std::fs::read(&path).unwrap()[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn fallback_chain_pins_bundled_hack_last() {
        let configured = vec!["Menlo".to_string(), "Apple Color Emoji".to_string()];

        let chain = fallback_chain("JetBrains Mono", &configured);
        assert_eq!(chain[..2], ["Menlo", "Apple Color Emoji"]);
        assert_eq!(chain.last().unwrap(), "Hack");

        let chain = fallback_chain("Hack", &configured);
        assert_eq!(chain[..2], ["Menlo", "Apple Color Emoji"]);
        assert!(!chain.iter().any(|f| f == "Hack"));

        let with_hack = vec!["Hack".to_string(), "Menlo".to_string()];
        let chain = fallback_chain("SF Mono", &with_hack);
        assert_eq!(chain[..2], ["Hack", "Menlo"]);

        assert_eq!(
            fallback_chain("Hack Nerd Font", &[]).last().unwrap(),
            "Hack",
            "a Hack-prefixed family name must not suppress the bundled anchor"
        );
    }

    #[test]
    fn fallback_chain_appends_platform_stock_faces() {
        let stock = crate::core::config::platform_last_resort_fallbacks();
        assert!(!stock.is_empty(), "every platform needs a CJK last resort");

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

        assert_eq!(chain[..legacy.len()], legacy[..]);

        let explicit = vec![stock[0].to_string()];
        let chain = fallback_chain("Hack", &explicit);
        assert_eq!(
            chain.iter().filter(|f| *f == stock[0]).count(),
            1,
            "stock face duplicated in {chain:?}"
        );

        assert!(!fallback_chain(stock[0], &[]).iter().any(|f| f == stock[0]));
    }

    #[test]
    fn wheel_routes_by_negotiated_mode_with_reporting_first() {
        let mouse = TermMode::MOUSE_REPORT_CLICK;
        assert_eq!(
            wheel_route(mouse, false, true),
            WheelRoute::Report { base: 64 }
        );
        assert_eq!(
            wheel_route(mouse, false, false),
            WheelRoute::Report { base: 65 }
        );

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

        assert_eq!(
            wheel_route(mouse | alt, false, true),
            WheelRoute::Report { base: 64 }
        );

        assert_eq!(
            wheel_route(TermMode::empty(), false, true),
            WheelRoute::Scrollback
        );
    }

    #[test]
    fn wheel_ignores_alternate_scroll_outside_the_alt_screen() {
        assert_eq!(
            wheel_route(TermMode::ALTERNATE_SCROLL, false, true),
            WheelRoute::Scrollback
        );
    }

    #[test]
    fn shift_wheel_always_scrolls_the_local_scrollback() {
        let everything = TermMode::MOUSE_MOTION
            | TermMode::ALT_SCREEN
            | TermMode::ALTERNATE_SCROLL
            | TermMode::APP_CURSOR;
        assert_eq!(wheel_route(everything, true, true), WheelRoute::Scrollback);
        assert_eq!(wheel_route(everything, true, false), WheelRoute::Scrollback);
    }

    #[test]
    fn right_click_opens_the_host_menu_when_the_app_is_not_reporting_mouse() {
        assert!(should_show_context_menu(false, false));
        assert!(should_show_context_menu(false, true));
    }

    #[test]
    fn right_click_belongs_to_the_app_while_mouse_reporting_is_active() {
        assert!(
            !should_show_context_menu(true, false),
            "an unmodified right-click in mouse mode is the application's event, \
             so tty7 must not also pop its own menu over it"
        );
    }

    #[test]
    fn shift_right_click_stays_the_escape_hatch_in_mouse_mode() {
        assert!(
            should_show_context_menu(true, true),
            "Shift is the documented way to reach tty7's own menu inside a \
             mouse-reporting TUI"
        );
    }

    #[test]
    fn context_menu_and_mouse_report_never_fire_for_the_same_click() {
        // The render-time menu gate and the mouse-down forwarder in
        // `TerminalElement` read the same predicate from opposite sides, so
        // every (mouse_mode, shift) pair must land in exactly one of them.
        for mouse_mode in [false, true] {
            for shift in [false, true] {
                let menu = should_show_context_menu(mouse_mode, shift);
                let reported = mouse_mode && !shift;
                assert_ne!(
                    menu, reported,
                    "mouse_mode={mouse_mode} shift={shift} must route to exactly one consumer"
                );
            }
        }
    }

    #[test]
    fn copy_on_select_copies_the_buffer_the_gesture_touched() {
        assert_eq!(select_end_copy(false, true, false), SelectEndCopy::None);
        assert_eq!(select_end_copy(false, false, true), SelectEndCopy::None);

        assert_eq!(select_end_copy(true, true, false), SelectEndCopy::Grid);
        assert_eq!(select_end_copy(true, false, true), SelectEndCopy::Editor);

        assert_eq!(select_end_copy(true, false, false), SelectEndCopy::None);

        assert_eq!(select_end_copy(true, true, true), SelectEndCopy::Grid);
    }

    #[test]
    fn sgr_mouse_reports_one_based_decimal_with_modifier_bits() {
        let plain = Modifiers::default();
        assert_eq!(
            encode_mouse(true, 0, &plain, 4, 8, true).unwrap(),
            b"\x1b[<0;5;9M".to_vec()
        );
        assert_eq!(
            encode_mouse(true, 2, &plain, 4, 8, false).unwrap(),
            b"\x1b[<2;5;9m".to_vec()
        );
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
        assert_eq!(
            encode_mouse(true, 64, &plain, 10, 3, true).unwrap(),
            b"\x1b[<64;11;4M".to_vec()
        );
        assert_eq!(
            encode_mouse(true, 35, &plain, 1, 1, true).unwrap(),
            b"\x1b[<35;2;2M".to_vec()
        );
    }

    #[test]
    fn sgr_mouse_has_no_coordinate_cap() {
        let plain = Modifiers::default();
        assert_eq!(
            encode_mouse(true, 0, &plain, 500, 300, true).unwrap(),
            b"\x1b[<0;501;301M".to_vec()
        );
    }

    #[test]
    fn x10_mouse_packs_bytes_and_drops_button_on_release() {
        let plain = Modifiers::default();
        assert_eq!(
            encode_mouse(false, 0, &plain, 4, 8, true).unwrap(),
            vec![0x1b, b'[', b'M', 32, 32 + 1 + 4, 32 + 1 + 8]
        );
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

    #[test]
    fn x10_mouse_drops_out_of_range_coordinates_whole() {
        let plain = Modifiers::default();
        assert!(encode_mouse(false, 0, &plain, 223, 0, true).is_none());
        assert!(encode_mouse(false, 0, &plain, 0, 223, true).is_none());
        let last = encode_mouse(false, 0, &plain, 222, 222, true).unwrap();
        assert_eq!(&last[4..], &[255, 255]);
    }

    #[test]
    fn fig_icon_emoji_takes_bare_emoji_and_template_badge_only() {
        assert_eq!(fig_icon_emoji("⚙️"), Some("⚙️"));
        assert_eq!(
            fig_icon_emoji("fig://template?color=2ecc71&badge=🔥"),
            Some("🔥")
        );
        assert_eq!(fig_icon_emoji("fig://icon?type=git"), None);
        assert_eq!(fig_icon_emoji("fig://template?color=2ecc71"), None);
        assert_eq!(fig_icon_emoji(""), None);
    }

    #[test]
    fn fig_icon_glyph_maps_known_types_and_falls_back_otherwise() {
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
        assert!(fig_icon_glyph("fig://icon?type=docker").is_none());
        assert!(fig_icon_glyph("⚙️").is_none());
    }

    #[test]
    fn focus_reports_only_when_the_app_opted_in() {
        assert_eq!(focus_report_bytes(TermMode::empty(), true), None);
        assert_eq!(focus_report_bytes(TermMode::empty(), false), None);
        let mode = TermMode::FOCUS_IN_OUT;
        assert_eq!(focus_report_bytes(mode, true), Some(b"\x1b[I".as_slice()));
        assert_eq!(focus_report_bytes(mode, false), Some(b"\x1b[O".as_slice()));
        assert_eq!(focus_report_bytes(TermMode::MOUSE_MOTION, true), None);
    }

    #[test]
    fn smooth_scroll_step_accumulates_and_clamps() {
        assert_eq!(smooth_scroll_step(0, 0.0, 0.4, 100), (0, 0.4));
        let (jump, frac) = smooth_scroll_step(0, 0.4, 0.8, 100);
        assert_eq!(jump, 1);
        assert!((frac - 0.2).abs() < 1e-4);
        let (jump, frac) = smooth_scroll_step(5, 0.2, -0.5, 100);
        assert_eq!(jump, -1);
        assert!((frac - 0.7).abs() < 1e-4);
        assert_eq!(smooth_scroll_step(3, 0.5, -10.0, 100), (-3, 0.0));
        assert_eq!(smooth_scroll_step(98, 0.0, 7.3, 100), (2, 0.0));
        assert_eq!(smooth_scroll_step(0, 0.0, 2.5, 0), (0, 0.0));
    }

    #[test]
    fn scroll_anim_step_converges_and_lands() {
        let frame = SCROLL_ANIM_FRAME;
        // A notch is spread over frames instead of being applied whole.
        let (step, last) = scroll_anim_step(3.0, frame);
        assert!(!last);
        assert!(
            step > 0. && step < 3.0,
            "took {step} of 3 lines in one frame"
        );

        // And it converges: no notch is left hanging.
        let mut remaining = 3.0_f32;
        let mut frames = 0u32;
        loop {
            let (step, last) = scroll_anim_step(remaining, frame);
            remaining -= step;
            frames += 1;
            if last {
                break;
            }
            assert!(frames < 200, "still {remaining} lines short after {frames}");
        }
        assert!(remaining.abs() < 1e-4, "landed {remaining} lines off");
        // Slow enough to read as motion, fast enough not to feel like lag.
        let ms = frames * SCROLL_ANIM_FRAME.as_millis() as u32;
        assert!((60..=200).contains(&ms), "a 3-line notch took {ms}ms");
    }

    #[test]
    fn scroll_anim_step_covers_dropped_frames() {
        // A late tick has to make up the ground it missed, or a busy pane would
        // scroll slower than an idle one.
        let (one, _) = scroll_anim_step(10.0, SCROLL_ANIM_FRAME);
        let (four, _) = scroll_anim_step(10.0, SCROLL_ANIM_FRAME * 4);
        assert!(four > one * 2., "{four} should far outpace {one}");
        // But the catch-up is capped, so a tick after a long stall never
        // overshoots what was actually asked for.
        let (stalled, _) = scroll_anim_step(10.0, std::time::Duration::from_secs(5));
        assert!(
            stalled > four && stalled <= 10.0,
            "stalled tick took {stalled}"
        );
    }

    #[test]
    fn scroll_anim_step_lands_on_a_negligible_remainder() {
        let (step, last) = scroll_anim_step(-0.005, SCROLL_ANIM_FRAME);
        assert!(last);
        assert_eq!(step, -0.005);
    }

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
        assert_eq!(trim_trailing_spaces("a  \nb\t\nc"), "a\nb\nc");
        assert_eq!(trim_trailing_spaces("a  \n"), "a\n");
        assert_eq!(trim_trailing_spaces("a  "), "a");
        assert_eq!(trim_trailing_spaces("  a  "), "  a");
    }

    #[test]
    fn paste_bytes_strips_esc_to_prevent_bracketed_paste_escape() {
        assert_eq!(
            paste_bytes("ls -la", true),
            b"\x1b[200~ls -la\x1b[201~".to_vec()
        );

        let evil = "foo\x1b[201~\nrm -rf ~\n";
        let out = paste_bytes(evil, true);
        let end = b"\x1b[201~";
        let markers = out.windows(end.len()).filter(|w| *w == end).count();
        assert_eq!(markers, 1);
        let inner = &out[b"\x1b[200~".len()..out.len() - end.len()];
        assert!(!inner.contains(&0x1b));
        assert_eq!(inner, b"foo[201~\nrm -rf ~\n");

        assert_eq!(paste_bytes("a\x1b[201~b", false), b"a\x1b[201~b".to_vec());
    }

    #[test]
    fn paste_bytes_normalizes_newlines_to_cr_without_bracketed_paste() {
        assert_eq!(paste_bytes("a\nb\r\nc\n", false), b"a\rb\rc\r".to_vec());
        assert_eq!(
            paste_bytes("a\nb", true),
            b"\x1b[200~a\nb\x1b[201~".to_vec()
        );
    }

    #[test]
    fn paste_bytes_folds_crlf_so_a_windows_clipboard_pastes_like_any_other() {
        assert_eq!(
            paste_bytes("a\r\nb\r\n", true),
            b"\x1b[200~a\nb\n\x1b[201~".to_vec(),
            "CRLF must reach the app as one line break, not two"
        );
        assert_eq!(
            paste_bytes("a\r\nb", true),
            paste_bytes("a\nb", true),
            "a Windows clipboard must paste exactly like a Unix one"
        );
    }

    #[test]
    fn submit_bytes_sends_a_multi_line_command_as_one_bracketed_paste() {
        assert_eq!(
            submit_bytes("echo a\necho b\necho c", true),
            b"\x1b[200~echo a\necho b\necho c\x1b[201~\r".to_vec()
        );
        let out = submit_bytes("a\nb\nc\nd", true);
        assert_eq!(out.iter().filter(|&&b| b == b'\r').count(), 1);
        assert_eq!(
            submit_bytes("ls -la", true),
            b"\x1b[200~ls -la\x1b[201~\r".to_vec()
        );
    }

    #[test]
    fn submit_bytes_falls_back_to_per_line_cr_without_bracketed_paste() {
        assert_eq!(submit_bytes("a\nb", false), b"a\rb\r".to_vec());
        assert_eq!(submit_bytes("a\r\nb", false), b"a\rb\r".to_vec());
    }

    #[test]
    fn submit_bytes_normalizes_line_breaks_inside_the_paste() {
        assert_eq!(
            submit_bytes("a\r\nb", true),
            b"\x1b[200~a\nb\x1b[201~\r".to_vec()
        );
        assert_eq!(
            submit_bytes("a\rb", true),
            b"\x1b[200~a\nb\x1b[201~\r".to_vec()
        );
        assert_eq!(submit_bytes("a\rb", false), b"a\rb\r".to_vec());
    }

    #[test]
    fn submit_bytes_strips_esc_and_skips_markers_on_an_empty_line() {
        let out = submit_bytes("foo\x1b[201~\nrm -rf ~", true);
        let end = b"\x1b[201~";
        assert_eq!(out.windows(end.len()).filter(|w| *w == end).count(), 1);
        assert_eq!(out, b"\x1b[200~foo[201~\nrm -rf ~\x1b[201~\r".to_vec());
        assert_eq!(submit_bytes("a\x1bb", false), b"ab\r".to_vec());

        assert_eq!(submit_bytes("", true), b"\r".to_vec());
    }

    #[test]
    fn shell_escape_path_escapes_spaces_and_metachars() {
        assert_eq!(
            shell_escape_path("/Users/me/notes.txt"),
            "/Users/me/notes.txt"
        );
        assert_eq!(
            shell_escape_path("/Users/me/My File (1).txt"),
            "/Users/me/My\\ File\\ \\(1\\).txt"
        );
        assert_eq!(
            shell_escape_path("/a/$HOME & more"),
            "/a/\\$HOME\\ \\&\\ more"
        );
        assert_eq!(shell_escape_path(""), "''");
        assert_eq!(shell_escape_path("a\nb"), "'a\nb'");
    }

    #[test]
    fn escape_candidate_quotes_what_the_shell_would_resplit() {
        assert_eq!(escape_candidate("notes.txt"), "notes.txt");
        assert_eq!(escape_candidate("--message"), "--message");
        assert_eq!(escape_candidate("My Documents"), "My\\ Documents");
        assert_eq!(escape_candidate("a(1)&b"), "a\\(1\\)\\&b");
        assert_eq!(
            escape_candidate("~/My Documents"),
            "~/My\\ Documents",
            "a leading ~/ is the user's own text and must stay expandable"
        );
        assert_eq!(
            escape_candidate("~weird name"),
            "\\~weird\\ name",
            "a bare ~ that is not a home prefix is just a filename character"
        );
    }

    #[test]
    fn clipboard_paste_text_escapes_and_space_joins_files() {
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
        assert_eq!(display_width('你'), 2);
        assert_eq!(display_width('한'), 2);
        assert_eq!(display_width('あ'), 2);
        assert_eq!(display_width('　'), 2);
    }

    #[test]
    fn display_width_emoji_are_wide() {
        assert_eq!(display_width('🚀'), 2);
        assert_eq!(display_width('🎉'), 2);
    }

    #[test]
    fn display_width_latin_accents_stay_narrow() {
        assert_eq!(display_width('é'), 1);
        assert_eq!(display_width('©'), 1);
        assert_eq!(display_width('±'), 1);
    }

    fn click(text: &str, scol: usize, cols: usize, col: usize, row: usize) -> Option<usize> {
        let chars: Vec<char> = text.chars().collect();
        wrapped_click_index(&chars, scol, cols, col, row, false)
    }

    #[test]
    fn wrapped_click_index_hits_chars_on_the_first_row() {
        assert_eq!(click("git", 4, 80, 4, 0), Some(0));
        assert_eq!(click("git", 4, 80, 6, 0), Some(2));
        assert_eq!(click("git", 4, 80, 1, 0), Some(0));
        assert_eq!(click("git", 4, 80, 40, 0), Some(3));
    }

    #[test]
    fn wrapped_click_index_maps_wrapped_rows() {
        assert_eq!(click("abcdef", 8, 10, 9, 0), Some(1));
        assert_eq!(click("abcdef", 8, 10, 0, 1), Some(2));
        assert_eq!(click("abcdef", 8, 10, 3, 1), Some(5));
        assert_eq!(click("a你", 2, 4, 3, 0), Some(1));
        assert_eq!(click("abcdef", 8, 10, 9, 1), Some(6));
    }

    #[test]
    fn wrapped_click_index_respects_wide_chars() {
        assert_eq!(click("你好", 2, 80, 2, 0), Some(0));
        assert_eq!(click("你好", 2, 80, 3, 0), Some(0));
        assert_eq!(click("你好", 2, 80, 4, 0), Some(1));
        assert_eq!(click("你", 4, 5, 0, 1), Some(0));
        assert_eq!(click("你", 4, 5, 1, 1), Some(0));
    }

    #[test]
    fn wrapped_click_index_rows_past_the_input_need_clamp() {
        let chars: Vec<char> = "ls".chars().collect();
        assert_eq!(wrapped_click_index(&chars, 4, 80, 3, 2, false), None);
        assert_eq!(wrapped_click_index(&chars, 4, 80, 3, 2, true), Some(2));
        assert_eq!(wrapped_click_index(&[], 4, 80, 30, 0, false), Some(0));
        assert_eq!(wrapped_click_index(&chars, 4, 80, 3, 1, false), None);
    }

    #[test]
    fn wrapped_click_index_covers_the_wrapped_caret_slot() {
        assert_eq!(click("abcdef", 4, 10, 0, 1), Some(6));
        assert_eq!(click("abcdef", 4, 10, 7, 1), Some(6));
        let chars: Vec<char> = "abcdef".chars().collect();
        assert_eq!(wrapped_click_index(&chars, 4, 10, 0, 2, false), None);
    }

    #[test]
    fn wrapped_click_index_treats_newlines_as_hard_breaks() {
        assert_eq!(click("a\nbc", 4, 80, 4, 0), Some(0));
        assert_eq!(click("a\nbc", 4, 80, 0, 1), Some(2));
        assert_eq!(click("a\nbc", 4, 80, 1, 1), Some(3));
        assert_eq!(click("a\nbc", 4, 80, 40, 0), Some(1));
        assert_eq!(click("a\nbc", 4, 80, 40, 1), Some(4));
        assert_eq!(click("a\n\nb", 4, 80, 3, 1), Some(2));
        assert_eq!(click("a\n\nb", 4, 80, 0, 2), Some(3));
    }

    #[test]
    fn input_overlay_rows_counts_wraps_slot_marked_and_newlines() {
        let rows = |text: &str, cursor: usize, marked: &str, scol: usize, cols: usize| {
            let chars: Vec<char> = text.chars().collect();
            input_overlay_rows(&chars, cursor, marked, scol, cols)
        };
        assert_eq!(rows("", 0, "", 3, 8), (1, 0));
        assert_eq!(rows("aaaaaaaaaa", 10, "", 6, 8), (3, 2));
        assert_eq!(rows("aaaaaaaaaa", 3, "", 6, 8), (2, 1));
        assert_eq!(rows("ab\ncd", 5, "", 0, 8), (2, 1));
        assert_eq!(rows("ab", 1, "漢", 6, 8), (2, 1));
    }

    #[test]
    fn input_overflow_shift_keeps_the_tail_and_caret_visible() {
        assert_eq!(input_overflow_shift(5, 2, 3, 22), 0);
        assert_eq!(input_overflow_shift(20, 2, 3, 22), 1);
        assert_eq!(input_overflow_shift(21, 29, 30, 22), 29);
        assert_eq!(input_overflow_shift(21, 0, 30, 22), 21);
    }

    #[test]
    fn a_description_that_fits_is_left_alone() {
        assert_eq!(elide("Show commit logs", 40), "Show commit logs");
        // Exactly the budget is still a fit; nothing is spent on an ellipsis.
        assert_eq!(elide("abcd", 4), "abcd");
    }

    #[test]
    fn an_overlong_description_ends_in_an_ellipsis_inside_its_budget() {
        let out = elide("Move or rename a file, a directory, or a symlink", 12);
        assert_eq!(out.chars().count(), 12, "the ellipsis is inside the budget");
        assert!(out.ends_with('…'));
        assert!(out.starts_with("Move or ren"));
    }

    #[test]
    fn a_budget_with_no_room_to_say_anything_says_nothing() {
        // A lone "…" reads as a description that is there but unreadable.
        assert_eq!(elide("Show commit logs", 1), "");
        assert_eq!(elide("Show commit logs", 0), "");
    }

    #[test]
    fn the_description_budget_is_what_the_name_leaves_of_the_menu() {
        const W: f32 = COMPLETION_MENU_MAX_W;
        // A 9px cell: 46px of row chrome, so a bare row leaves (480-46)/9 cells.
        assert_eq!(description_budget(9., 0, W), 48);
        // Every cell the name takes is one the description does not get.
        assert_eq!(description_budget(9., 10, W), 38);
        // A name that fills the menu on its own leaves nothing, and never
        // underflows into a huge budget.
        assert_eq!(description_budget(9., 200, W), 0);
        assert_eq!(description_budget(0., 10, W), 0);
        // A pane too narrow for the full menu shrinks the budget with it,
        // instead of eliding to a width the menu never got.
        assert!(description_budget(9., 10, 240.) < description_budget(9., 10, W));
        assert_eq!(description_budget(9., 10, 240.), 11);
    }

    #[test]
    fn menu_layout_prefers_below_and_flips_above_when_cramped() {
        assert_eq!(menu_layout(24, 3, 5, 0, 10), (false, 5, 0));
        assert_eq!(menu_layout(24, 22, 5, 0, 10), (true, 5, 0));
        assert_eq!(menu_layout(6, 4, 10, 0, 10), (true, 2, 0));
        assert_eq!(menu_layout(6, 1, 10, 0, 10), (false, 2, 0));
        let (_, visible, _) = menu_layout(1, 0, 8, 0, 10);
        assert_eq!(visible, 1);
    }

    #[test]
    fn menu_layout_budgets_the_overflow_footers() {
        let (place_above, visible, first) = menu_layout(24, 13, 30, 17, 10);
        assert!(
            place_above,
            "12 needed lines don't fit in the 10 rows below"
        );
        assert_eq!(visible, 10);
        assert!((first..first + visible).contains(&17));
    }

    #[test]
    fn menu_layout_caps_rows_and_windows_around_the_selection() {
        let (_, visible, first) = menu_layout(40, 0, 30, 17, 10);
        assert_eq!(visible, 10);
        assert!((first..first + visible).contains(&17));
        assert_eq!(first, 8);
        let (_, visible, first) = menu_layout(40, 0, 30, 29, 10);
        assert_eq!(first, 20);
        assert_eq!(first + visible, 30);
        assert_eq!(menu_layout(40, 0, 30, 3, 10).2, 0);
    }

    #[test]
    fn a_multiline_entry_draws_on_one_menu_row() {
        // A command composed in the inline editor keeps its newlines in the
        // in-session history. Left alone, gpui breaks the row's text on them
        // and the tail paints over every row below it.
        let line = "curl 'http://x' \\\n  -H 'accept: */*' \\\n  -H 'pragma: no-cache'";
        let runs = highlight_runs(line, &[0, 1, 2, 3]);
        let drawn: String = runs.iter().map(|(run, _)| run.as_str()).collect();
        assert!(
            !drawn.contains('\n'),
            "no run carries a line break: {drawn}"
        );
        assert_eq!(drawn.chars().count(), line.chars().count());
        assert_eq!(drawn.matches('↵').count(), 2);
        assert_eq!(
            runs.first().map(|(r, hit)| (r.as_str(), *hit)),
            Some(("curl", true))
        );
        assert!(runs[1..].iter().all(|(_, hit)| !hit));
    }

    #[test]
    fn highlight_runs_alternate_on_the_matched_characters() {
        let runs = highlight_runs("git status", &[0, 4, 5]);
        let shape: Vec<(&str, bool)> = runs.iter().map(|(r, h)| (r.as_str(), *h)).collect();
        assert_eq!(
            shape,
            [("g", true), ("it ", false), ("st", true), ("atus", false)]
        );
        assert!(highlight_runs("", &[]).is_empty());
    }

    #[test]
    fn only_a_matching_host_may_answer_for_a_panes_paths() {
        assert!(cwd_is_on_host(false, true));
        assert!(cwd_is_on_host(true, false));

        assert!(!cwd_is_on_host(true, true));
        assert!(!cwd_is_on_host(false, false));
    }

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

        let remote = ws.target.host_id();
        assert_eq!(remote, target.host_id(), "the workspace's own machine");
        assert!(!remote.is_local(), "a remote workspace is not this machine");
        assert_eq!(
            HostId::from_connection_key("ssh-alias:build-box"),
            remote,
            "the id the connection was opened under, or the registry lookup misses"
        );

        let sibling = PaneWorkspace {
            workspace: WorkspaceId::new(),
            target,
            spec: None,
        };
        assert_eq!(sibling.target.host_id(), remote);
    }
}

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

#[cfg(all(test, unix))]
pub(crate) fn quiet_test_ssh_pane(
    pane_id: u64,
    window: &mut Window,
    cx: &mut gpui::App,
) -> (gpui::Entity<TerminalView>, std::os::unix::net::UnixStream) {
    let (view, stream) = quiet_test_pane(pane_id, window, cx);
    view.update(cx, |view, _| {
        view.ssh_spec = Some(Box::new(
            serde_json::from_str(
                r#"{"host":"build-box","port":22,"user":"me","auth_mode":"auto"}"#,
            )
            .expect("a minimal NativeSshSpec decodes"),
        ));
    });
    (view, stream)
}

#[cfg(all(test, unix))]
mod gpui_tests {
    use super::*;
    use crate::daemon::protocol::{ClientMsg, DaemonMsg};
    use gpui::{Entity, TestAppContext, point};
    use std::os::unix::net::UnixStream;

    fn harness(cx: &mut TestAppContext) -> (gpui::WindowHandle<TerminalView>, UnixStream) {
        // Building a view reads the config. Whether that hit the real user
        // directory used to come down to which test happened to pin the
        // scratch dir first.
        crate::core::config::pin_test_config_dir();
        cx.executor().allow_parking();
        let (client_side, daemon_side) = UnixStream::pair().unwrap();
        cx.update(|cx| {
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

    /// The same pane, but hung under a `gpui_component::Root` the way the real
    /// window hangs it.
    ///
    /// `harness` makes the view its own root, which is enough for anything
    /// that never paints — but gpui-component's text input reaches for `Root`
    /// while painting, so any test that lets a frame draw with the search bar
    /// (or any other input) on screen needs this one instead.
    fn rooted_harness(
        cx: &mut TestAppContext,
    ) -> (
        gpui::WindowHandle<gpui_component::Root>,
        Entity<TerminalView>,
        UnixStream,
    ) {
        crate::core::config::pin_test_config_dir();
        cx.executor().allow_parking();
        let (client_side, daemon_side) = UnixStream::pair().unwrap();
        cx.update(|cx| {
            gpui_component::init(cx);
            cx.set_global(Config::default());
        });
        let built: std::rc::Rc<std::cell::RefCell<Option<Entity<TerminalView>>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let out = built.clone();
        let window = cx.add_window(move |window, cx| {
            let terminal = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24))
                .expect("socketpair-backed terminal");
            let view = cx.new(|cx| TerminalView::with_terminal(terminal, 1, window, cx));
            *out.borrow_mut() = Some(view.clone());
            gpui_component::Root::new(view, window, cx)
        });
        window
            .update(cx, |_, window, _| window.activate_window())
            .unwrap();
        cx.background_executor.run_until_parked();
        let view = built.borrow_mut().take().expect("the pane was built");
        (window, view, daemon_side)
    }

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

    #[gpui::test]
    fn an_agent_that_has_finished_its_turn_is_not_busy(cx: &mut TestAppContext) {
        use crate::core::cli_agent::{AgentSessionState, AgentStatus, CLIAgent};

        crate::core::config::pin_test_config_dir();
        let (window, mut daemon) = harness(cx);
        DaemonMsg::Agent(Some(CLIAgent::Claude))
            .encode(&mut daemon)
            .unwrap();

        let report = |status: AgentStatus, daemon: &mut UnixStream| {
            DaemonMsg::AgentStatus(Some(AgentSessionState {
                status,
                message: None,
                session_id: Some("sid-abc".into()),
                launch_argv: Some(vec!["claude".into()]),
                rich: true,
                cwd: None,
                activity: 0,
            }))
            .encode(daemon)
            .unwrap();
        };
        let settled = |want: Option<PaneBusy>, cx: &mut TestAppContext| {
            for _ in 0..200 {
                if window.update(cx, |view, _, _| view.busy()).unwrap() == want {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            false
        };

        report(AgentStatus::Working, &mut daemon);
        assert!(
            settled(Some(PaneBusy::Agent("Claude Code")), cx),
            "a turn in flight is work closing would cut short"
        );

        // The green Done badge is what sends a reader to close the tab. Asking
        // "Claude Code is still working" there is both false and in the way.
        report(AgentStatus::Done, &mut daemon);
        assert!(
            settled(None, cx),
            "a finished turn must not hold the tab open"
        );
    }

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

    /// A 1x1 red placement anchored at an absolute scrollback row, built the way
    /// the decode worker hands one to the store.
    fn placed_at(anchor_row: i64) -> crate::terminal::images::PlacedImage {
        use tty7_core::core::kitty_graphics::{Image, WireFormat};

        let mut img = Image {
            id: 1,
            number: 0,
            placement: 0,
            width: 1,
            height: 1,
            cols: 0,
            rows: 0,
            data: vec![0xff, 0x00, 0x00, 0xff],
            format: WireFormat::Rgba,
            compressed: false,
        };
        let (data, width_px, height_px) = crate::terminal::images::decode(&mut img).unwrap();
        crate::terminal::images::PlacedImage {
            data,
            anchor_row,
            anchor_col: 0,
            width_px,
            height_px,
            cols: 0,
            rows: 0,
            id: 1,
            placement: 0,
            painted: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    #[gpui::test]
    fn clearing_the_scrollback_drops_what_was_anchored_in_it(cx: &mut TestAppContext) {
        let (window, mut daemon) = harness(cx);

        // Overflow the 24-row viewport so there is a scrollback to purge.
        let mut out = Vec::new();
        for i in 0..60 {
            out.extend_from_slice(format!("line {i}\r\n").as_bytes());
        }
        DaemonMsg::Output(out).encode(&mut daemon).unwrap();
        for _ in 0..200 {
            let filled = window
                .update(cx, |view, _, _| {
                    view.terminal.term.lock().grid().history_size() > 0
                })
                .unwrap();
            if filled {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        window
            .update(cx, |view, _, cx| {
                let history = view.terminal.term.lock().grid().history_size();
                assert!(history > 0, "the test needs a scrollback to clear");

                // Both of these address rows `clear_history` is about to drop:
                // an image anchored by absolute row, and a selection reaching up
                // into the history.
                view.terminal.images().place(placed_at(history as i64));
                view.terminal.term.lock().selection = Some(Selection::new(
                    SelectionType::Simple,
                    Point::new(Line(-1), Column(0)),
                    Side::Left,
                ));

                view.clear_scrollback(cx);

                assert!(
                    view.terminal.images().snapshot().is_empty(),
                    "a stale anchor blits the frame over live output, or off-screen \
                     entirely — the daemon never replays the frame to correct it"
                );
                assert!(
                    view.terminal.term.lock().selection.is_none(),
                    "a selection left pointing at purged rows clamps onto the \
                     viewport and copies whatever text moved into them"
                );
            })
            .unwrap();
    }

    #[gpui::test]
    fn the_scrollbar_moves_the_viewport_and_follows_it_back(cx: &mut TestAppContext) {
        use gpui_component::scroll::ScrollbarHandle as _;

        let (window, mut daemon) = harness(cx);

        // Overflow the 24-row viewport so there is a scrollback to scroll.
        let mut out = Vec::new();
        for i in 0..60 {
            out.extend_from_slice(format!("line {i}\r\n").as_bytes());
        }
        DaemonMsg::Output(out).encode(&mut daemon).unwrap();
        // Wait for the reader to go quiet, not just to start: a scrollback
        // still filling underneath would move every row this test names.
        let mut settled = 0;
        for _ in 0..200 {
            let now = window
                .update(cx, |view, _, _| {
                    view.terminal.term.lock().grid().history_size()
                })
                .unwrap();
            if now > 0 && now == settled {
                break;
            }
            settled = now;
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        window
            .update(cx, |view, _, _| {
                view.sync_scrollbar();
                let history = view.terminal.term.lock().grid().history_size();
                assert!(history > 0, "the test needs a scrollback to scroll");
                let row = view.line_height.as_f32();
                assert_eq!(
                    view.scroll_handle.offset().y,
                    px(-(history as f32) * row),
                    "at the live edge the whole scrollback sits above the viewport"
                );

                // Drag the thumb a third of the way up its track. The bar only
                // records the row; the pane applies it on its next render.
                view.scroll_frac = 0.5;
                view.scroll_handle
                    .set_offset(point(px(0.), px(-(history as f32) * row / 3.)));
                assert_eq!(
                    view.terminal.term.lock().grid().display_offset(),
                    0,
                    "the bar does not reach into the terminal itself"
                );

                view.sync_scrollbar();
                let offset = view.terminal.term.lock().grid().display_offset();
                assert_eq!(
                    offset,
                    history - (history as f32 / 3.).round() as usize,
                    "the viewport lands on the row the thumb was dropped on"
                );
                assert_eq!(
                    view.scroll_frac, 0.,
                    "a sub-line remainder left over from the wheel would paint \
                     the grid off the row the thumb picked"
                );
                assert_eq!(
                    view.scroll_handle.offset().y,
                    px(-((history - offset) as f32) * row),
                    "and the thumb reports the row the grid actually reached"
                );
            })
            .unwrap();
    }

    #[gpui::test]
    fn a_stale_hover_row_does_not_index_the_shrunken_grid(cx: &mut TestAppContext) {
        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, _, cx| {
                view.hover_link_at(0, 23, true, cx);
                view.terminal.resize(TermSize::new(80, 8), 8, 17);
                view.last_hover_cell = Some((0, 23));
                assert!(
                    !view.refresh_link_hover(true, cx),
                    "a row outside the grid can't hold a link"
                );
            })
            .unwrap();
    }

    #[gpui::test]
    fn a_resize_forgets_the_hovered_cell(cx: &mut TestAppContext) {
        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, _, cx| {
                view.set_grid_size(80, 24, px(8.), px(17.), 1.);
                view.hover_link_at(0, 23, true, cx);
                assert_eq!(view.last_hover_cell, Some((0, 23)));
                view.hovered_link = Some(HoveredLink {
                    start: Point::new(Line(23), Column(0)),
                    end: Point::new(Line(23), Column(3)),
                });
                view.set_grid_size(80, 24, px(8.), px(17.), 1.);
                assert_eq!(view.last_hover_cell, Some((0, 23)));
                view.set_grid_size(80, 8, px(8.), px(17.), 1.);
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

    fn next_input(daemon: &mut UnixStream) -> Vec<u8> {
        loop {
            match ClientMsg::read(daemon).expect("client socket stays open") {
                ClientMsg::Input(bytes) => return bytes,
                _ => continue,
            }
        }
    }

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
    fn passthrough_ctrl_c_discards_typeahead_before_the_shell_can_resume(cx: &mut TestAppContext) {
        let (window, mut daemon) = harness(cx);
        window
            .update(cx, |view, window, cx| {
                assert!(!view.input_active(), "the foreground process owns input");
                view.typeahead.observe(RawInput::Text("agent input"), false);
                view.typeahead.observe(
                    RawInput::Key {
                        key: "up",
                        plain: true,
                    },
                    false,
                );

                view.on_key_down(
                    &KeyDownEvent {
                        keystroke: key("ctrl-c"),
                        is_held: false,
                        prefer_character_input: false,
                    },
                    window,
                    cx,
                );
                assert_eq!(view.typeahead.drain(), None);
                view.flush_typeahead();
            })
            .unwrap();

        assert_eq!(next_input_until_timeout(&mut daemon), Some(vec![0x03]));
        assert_eq!(
            next_input_until_timeout(&mut daemon),
            None,
            "resuming the shell must not synthesize Ctrl-U after Ctrl-C"
        );
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

    /// Handing the line back to the shell walks the cursor left once per
    /// character it sat before. Those are arrow keys like any other, so under
    /// DECCKM they have to be SS3 — and zsh's zle does turn DECCKM on, so this
    /// is the ordinary case rather than the exotic one.
    #[gpui::test]
    fn a_handoff_walks_the_cursor_back_in_ss3_under_app_cursor_mode(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let (window, mut daemon) = harness(cx);
        DaemonMsg::Output(b"\x1b[?1h".to_vec())
            .encode(&mut daemon)
            .unwrap();
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
                assert!(
                    view.key_flags().app_cursor(),
                    "the shell asked for application cursor keys"
                );
                for ch in ["z", "z", "q", "q", "x"] {
                    type_char(view, ch, window, cx);
                }
                view.handle_editor_key(&key("left"), cx);
                view.handle_editor_key(&key("left"), cx);
                view.complete_tab(true, cx);
            })
            .unwrap();
        assert_eq!(
            next_input_until_timeout(&mut daemon),
            Some(b"zzqqx".to_vec())
        );
        assert_eq!(
            next_input_until_timeout(&mut daemon),
            Some(b"\x1bOD\x1bOD".to_vec()),
            "the cursor walks back in SS3, not CSI"
        );
    }

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

    #[gpui::test]
    fn ctrl_c_after_tab_handoff_returns_the_fresh_prompt_to_the_editor(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
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
                for ch in ["z", "z", "q", "q", "x"] {
                    type_char(view, ch, window, cx);
                }
                view.complete_tab(true, cx);
                assert!(!view.input_active(), "Tab handed this line to the shell");
                view.on_key_down(
                    &KeyDownEvent {
                        keystroke: key("ctrl-c"),
                        is_held: false,
                        prefer_character_input: false,
                    },
                    window,
                    cx,
                );
            })
            .unwrap();
        assert_eq!(
            next_input_until_timeout(&mut daemon),
            Some(b"zzqqx".to_vec())
        );
        assert_eq!(next_input_until_timeout(&mut daemon), Some(b"\t".to_vec()));
        assert_eq!(next_input_until_timeout(&mut daemon), Some(vec![0x03]));

        // Tab handoff never emitted C, so the shell integration reports the
        // interrupted prompt as another A/B while the daemon is still at_prompt.
        DaemonMsg::Prompt {
            active: true,
            at_prompt: true,
            last_exit: Some(130),
        }
        .encode(&mut daemon)
        .unwrap();
        wait_for_input_active(&window, cx);

        window
            .update(cx, |view, window, cx| {
                type_char(view, "n", window, cx);
                assert_eq!(
                    view.cmd.text(),
                    "n",
                    "the first character on the fresh line belongs to tty7's editor"
                );
            })
            .unwrap();
        assert_eq!(
            next_input_until_timeout(&mut daemon),
            None,
            "the fresh line must not keep going raw to the shell"
        );
    }

    #[gpui::test]
    fn a_late_remote_listing_leaves_a_line_the_editor_no_longer_owns_alone(
        cx: &mut TestAppContext,
    ) {
        crate::core::config::pin_test_config_dir();
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
            .update(cx, |view, _, cx| {
                view.cmd.set("ls /nope/");
                view.editor_handoff = Some(view.terminal.prompt_cycle());
                assert!(!view.input_active(), "the shell owns this prompt already");

                let req =
                    super::completion::remote_path_request("ls /nope/", 9, "/home/u").unwrap();
                view.remote_path_results(req, "ls /nope/", 9, Vec::new(), true, cx);

                assert_eq!(
                    view.cmd.text(),
                    "ls /nope/",
                    "an empty listing must not hand off a line the editor no longer drives"
                );
            })
            .unwrap();
        assert_eq!(
            next_input_until_timeout(&mut daemon),
            None,
            "not one byte reached the wire"
        );
    }

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

    fn dir_candidate(text: &str, start: usize, end: usize) -> completion::Candidate {
        completion::Candidate {
            text: text.into(),
            kind: CandidateKind::Dir,
            start,
            end,
            description: None,
            icon: None,
        }
    }

    #[gpui::test]
    fn accepting_a_candidate_escapes_it_for_the_shell(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, _, _| {
                view.cmd.set("cd My");
                view.completion_insert(&dir_candidate("My Documents", 3, 5), 3);
                assert_eq!(
                    view.cmd.text(),
                    "cd My\\ Documents/",
                    "an unescaped candidate resplits into two arguments and the command breaks"
                );

                view.cmd.set("cd ~/My");
                view.completion_insert(&dir_candidate("~/My Documents", 3, 6), 3);
                assert_eq!(view.cmd.text(), "cd ~/My\\ Documents/");

                view.cmd.set("git commit --mess");
                view.completion_insert(
                    &completion::Candidate {
                        text: "--message".into(),
                        kind: CandidateKind::Flag,
                        start: 11,
                        end: 17,
                        description: None,
                        icon: None,
                    },
                    11,
                );
                assert_eq!(view.cmd.text(), "git commit --message ");
            })
            .unwrap();
    }

    #[gpui::test]
    fn a_candidate_needing_escapes_is_never_half_applied(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, _, cx| {
                view.cmd.set("cd My");
                let offered = view.offer_candidates(
                    "cd My",
                    3,
                    5,
                    vec![
                        dir_candidate("My Documents", 3, 5),
                        dir_candidate("My Music", 3, 5),
                    ],
                    0,
                    cx,
                );
                assert!(offered.is_some(), "two candidates open a menu");
                assert_eq!(
                    view.cmd.text(),
                    "cd My",
                    "the common prefix here is `My ` — writing it raw would break the line \
                     and the trailing space would close the menu on the next keystroke"
                );
            })
            .unwrap();
    }

    fn parsed(text: &str) -> super::super::generator::Parsed {
        super::super::generator::Parsed {
            text: text.into(),
            description: None,
        }
    }

    #[gpui::test]
    fn a_generator_that_supplies_no_match_closes_the_menu(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, _, cx| {
                // `git ckout<Tab>`: no subcommand matches, but git's alias generator
                // is in flight, so the session opens empty and waits for it.
                view.cmd.set("ckout");
                let generation =
                    view.open_completion(CompletionSession::new(0, "ckout".into(), Vec::new(), 1));
                assert!(
                    view.completion.is_some(),
                    "the menu waits for its generator"
                );

                view.completion_merge(generation, Vec::new(), cx);
                assert!(
                    view.completion.is_none(),
                    "a menu that never got a candidate must not stay armed — it swallows \
                     every later Tab instead of handing the line to the shell"
                );
            })
            .unwrap();
    }

    #[gpui::test]
    fn generator_results_that_match_nothing_close_the_menu(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, _, cx| {
                view.cmd.set("ckout");
                let generation =
                    view.open_completion(CompletionSession::new(0, "ckout".into(), Vec::new(), 1));

                view.completion_merge(generation, vec![parsed("main"), parsed("release")], cx);
                assert!(
                    view.completion.is_none(),
                    "branches that match nothing typed are as good as no candidates at all"
                );
            })
            .unwrap();
    }

    #[gpui::test]
    fn a_menu_waits_while_another_generator_is_still_in_flight(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, _, cx| {
                view.cmd.set("ck");
                let generation =
                    view.open_completion(CompletionSession::new(0, "ck".into(), Vec::new(), 2));

                view.completion_merge(generation, Vec::new(), cx);
                assert!(
                    view.completion.is_some(),
                    "one generator came back empty, the other has not answered yet"
                );

                view.completion_merge(generation, vec![parsed("ckout-fix")], cx);
                let s = view
                    .completion
                    .as_ref()
                    .expect("the second one supplied a match");
                assert_eq!(s.filtered.len(), 1);
            })
            .unwrap();
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

    #[gpui::test]
    fn shell_vi_mode_prompt_releases_gap_hold_without_stale_typeahead(cx: &mut TestAppContext) {
        let (window, mut daemon) = harness(cx);
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
        cx.executor().advance_clock(HOLD_WINDOW * 2);
        cx.run_until_parked();
        assert_eq!(
            next_input_until_timeout(&mut daemon),
            Some(b"ls".to_vec()),
            "gap text typed before a vi prompt must reach the shell"
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

    #[gpui::test]
    fn ctrl_r_without_integration_raises_the_notice_once(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();

        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, window, cx| {
                let ctrl_r = KeyDownEvent {
                    keystroke: key("ctrl-r"),
                    is_held: false,
                    prefer_character_input: false,
                };
                view.on_key_down(&ctrl_r, window, cx);
                assert!(
                    view.integration_notice.is_none(),
                    "the grace window stays silent"
                );

                view.created_at = std::time::Instant::now() - INTEGRATION_GRACE * 2;
                view.on_key_down(&ctrl_r, window, cx);
                assert!(
                    view.integration_notice.is_some(),
                    "Ctrl+R raises the notice"
                );
                cx.notify();
            })
            .unwrap();

        cx.run_until_parked();
        window
            .update(cx, |view, window, cx| {
                assert!(
                    view.integration_notice.is_some(),
                    "the notice survives a real render pass"
                );

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

    #[gpui::test]
    fn insert_newline_action_extends_the_line_and_enter_submits_it(cx: &mut TestAppContext) {
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
            Some(b"echo a\recho b\r".to_vec()),
            "the multi-line command reaches the PTY in one submit"
        );
    }

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
                view.cmd.set_with_cursor("git ", 4);
                view.open_completion(CompletionSession::new(
                    4,
                    String::new(),
                    vec![candidate("status")],
                    0,
                ));

                view.insert_newline_action(cx);
                assert!(
                    view.completion.is_none(),
                    "the newline ends the completed word, so the menu closes"
                );
                assert_eq!(view.cmd.text(), "git \n");

                view.cmd.set_with_cursor("git ", 4);
                view.open_completion(CompletionSession::new(
                    4,
                    String::new(),
                    vec![candidate("status")],
                    0,
                ));
                view.handle_editor_key(&key("enter"), cx);
                assert_eq!(view.cmd.text(), "git status ");
            })
            .unwrap();
    }

    #[gpui::test]
    fn shift_enter_reaches_a_foreground_tui_with_kitty_encoding(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let (window, mut daemon) = harness(cx);
        cx.update(|cx| crate::ui::keymap::init(cx));
        DaemonMsg::Output(b"\x1b[>1u".to_vec())
            .encode(&mut daemon)
            .unwrap();
        for _ in 0..200 {
            cx.run_until_parked();
            if window
                .update(cx, |view, _, _| view.key_flags().kitty_active())
                .unwrap()
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        window
            .update(cx, |view, window, cx| {
                assert!(!view.input_active(), "the foreground TUI owns input");
                window.activate_window();
                view.focus_handle.focus(window, cx);
            })
            .unwrap();

        let mut vcx = gpui::VisualTestContext::from_window(window.into(), cx);
        vcx.simulate_keystrokes("shift-enter");

        assert_eq!(
            next_input_until_timeout(&mut daemon),
            Some(b"\x1b[13;2u".to_vec())
        );
    }

    #[gpui::test]
    fn shift_enter_reaches_a_foreground_tui_as_lf_without_kitty(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let (window, mut daemon) = harness(cx);
        cx.update(|cx| crate::ui::keymap::init(cx));
        window
            .update(cx, |view, window, cx| {
                assert!(!view.input_active(), "the foreground TUI owns input");
                window.activate_window();
                view.focus_handle.focus(window, cx);
            })
            .unwrap();

        let mut vcx = gpui::VisualTestContext::from_window(window.into(), cx);
        vcx.simulate_keystrokes("shift-enter");

        assert_eq!(next_input_until_timeout(&mut daemon), Some(b"\n".to_vec()));
    }

    #[gpui::test]
    fn alt_enter_keeps_its_legacy_encoding_in_a_foreground_tui(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let (window, mut daemon) = harness(cx);
        cx.update(|cx| crate::ui::keymap::init(cx));
        window
            .update(cx, |view, window, cx| {
                assert!(!view.input_active(), "the foreground TUI owns input");
                window.activate_window();
                view.focus_handle.focus(window, cx);
            })
            .unwrap();

        let mut vcx = gpui::VisualTestContext::from_window(window.into(), cx);
        vcx.simulate_keystrokes("alt-enter");

        assert_eq!(
            next_input_until_timeout(&mut daemon),
            Some(b"\x1b\r".to_vec())
        );
    }

    #[gpui::test]
    fn insert_newline_action_declines_when_the_editor_is_not_live(cx: &mut TestAppContext) {
        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, _, cx| {
                view.cmd.set("keep me");
                view.terminal.exited = true;
                assert!(!view.input_active());
                view.insert_newline_action(cx);
                assert_eq!(view.cmd.text(), "keep me", "no newline inserted");
            })
            .unwrap();
    }

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

    #[gpui::test]
    fn ctrl_r_steps_matches_and_cmd_enter_runs(cx: &mut TestAppContext) {
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

    #[gpui::test]
    fn ctrl_j_and_ctrl_m_submit_the_line_like_enter(cx: &mut TestAppContext) {
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

    #[gpui::test]
    fn reverse_search_menu_survives_a_real_render_pass(cx: &mut TestAppContext) {
        let (window, mut daemon) = harness(cx);
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
        cx.run_until_parked();
        window
            .update(cx, |view, _, _| {
                assert!(view.reverse_search.is_some(), "search survives the frame");
            })
            .unwrap();
    }

    #[gpui::test]
    fn submitted_command_backfills_its_exit_code(cx: &mut TestAppContext) {
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
                view.handle_editor_key(&meta("b"), cx);
                assert_eq!(view.cmd.cursor(), 5);
                view.handle_editor_key(&meta("d"), cx);
                assert_eq!(view.cmd.text(), "echo ");
                view.handle_editor_key(&meta("b"), cx);
                assert_eq!(view.cmd.cursor(), 0);
                view.handle_editor_key(&meta("f"), cx);
                assert_eq!(view.cmd.cursor(), 4);
                view.handle_editor_key(&meta("z"), cx);
                assert_eq!(view.cmd.text(), "");
            })
            .unwrap();
    }

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

    /// Shaped after what macOS actually delivers, measured on a wheel mouse and
    /// a trackpad: both arrive as pixels, and only the trackpad ever reports a
    /// phase. One wheel detent is ~103px, roughly five lines at a 21px line
    /// height; a trackpad event is a fraction of that but can reach ~3 lines
    /// when flicked, which is why phase and not size decides.
    fn wheel(view: &TerminalView, lines: f32, phase: gpui::TouchPhase) -> ScrollWheelEvent {
        ScrollWheelEvent {
            delta: gpui::ScrollDelta::Pixels(point(px(0.), px(lines * view.line_height.as_f32()))),
            touch_phase: phase,
            ..Default::default()
        }
    }

    fn notch(view: &TerminalView, lines: f32) -> ScrollWheelEvent {
        wheel(view, lines, gpui::TouchPhase::Moved)
    }

    /// The whole point of the animation: a detent must not land in one go.
    #[gpui::test]
    fn a_wheel_notch_is_spread_over_frames(cx: &mut TestAppContext) {
        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, w, cx| {
                scroll_into_history(view, 10);
                let ev = notch(view, -4.9);
                view.on_scroll(&ev, w, cx);
                assert_eq!(
                    display_offset(view),
                    10,
                    "the detent was applied whole, before a single frame ran"
                );
                assert_eq!(view.scroll_frac, 0., "and not even a sliver of it");
                assert!(view.scroll_anim.is_some(), "nothing was left to animate");
            })
            .unwrap();
    }

    /// Zooming has to take the wheel away from the buffer entirely, or the
    /// grid would slide under the pointer while the font changed size.
    #[gpui::test]
    fn the_zoom_modifier_takes_the_wheel_off_the_scrollback(cx: &mut TestAppContext) {
        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, w, cx| {
                scroll_into_history(view, 10);
                let mut ev = notch(view, -4.9);
                ev.modifiers = Modifiers::secondary_key();
                view.on_scroll(&ev, w, cx);
                assert_eq!(display_offset(view), 10, "the wheel reached the grid");
                assert!(view.scroll_anim.is_none(), "and queued more of it");
            })
            .unwrap();
    }

    /// A detent is one step however many lines the platform bills it as —
    /// macOS calls a single notch five.
    #[test]
    fn a_wheel_detent_zooms_by_exactly_one_step() {
        assert_eq!(zoom_scroll_steps(4.9, 0., false), (1, 0.));
        assert_eq!(zoom_scroll_steps(-4.9, 0., false), (-1, 0.));
        assert_eq!(zoom_scroll_steps(0., 0., false), (0, 0.));
    }

    /// A trackpad has no detents, so a flick arrives as a stream of slivers.
    /// Paying out a step per sliver would run the font from end to end.
    #[test]
    fn a_trackpad_flick_adds_up_to_whole_steps() {
        let (mut debt, mut steps) = (0., 0);
        for _ in 0..20 {
            let (s, d) = zoom_scroll_steps(1. / 3., debt, true);
            steps += s;
            debt = d;
        }
        assert_eq!(steps, 2, "twenty thirds of a line is two steps, not twenty");
        assert!(debt > 0., "and the remainder was dropped instead of kept");
    }

    /// The one thing typing at a prompt must never do is land where the
    /// person typing cannot see it.
    #[gpui::test]
    fn typing_brings_the_view_back_to_the_prompt(cx: &mut TestAppContext) {
        let (window, mut daemon) = harness(cx);
        prompt_ready(&window, cx, &mut daemon);
        window
            .update(cx, |view, _w, cx| {
                assert!(
                    view.input_active(),
                    "this is the branch that was leaving the view parked"
                );
                scroll_into_history(view, 10);
                view.commit_text("l", cx);
                assert_eq!(
                    display_offset(view),
                    0,
                    "the character went in while the viewport stayed in the scrollback"
                );
                assert_eq!(view.cmd.text(), "l", "and it did reach the line");
            })
            .unwrap();
    }

    /// A paste is a larger change than a keystroke to make out of sight, and
    /// it went the same way.
    #[gpui::test]
    fn pasting_brings_the_view_back_to_the_prompt(cx: &mut TestAppContext) {
        let (window, mut daemon) = harness(cx);
        prompt_ready(&window, cx, &mut daemon);
        window
            .update(cx, |view, _w, cx| {
                scroll_into_history(view, 10);
                view.paste("cargo test\n".to_string(), cx);
                assert_eq!(display_offset(view), 0, "the paste landed off screen");
                assert_eq!(
                    view.cmd.text(),
                    "cargo test",
                    "and the trailing newline is still dropped"
                );
            })
            .unwrap();
    }

    /// A trackpad is already a continuous stream — animating it would only put
    /// lag between the fingers and the grid. It is told apart by its phase, not
    /// by its delta type or size: a flick moves further in one event than a
    /// slowly inched wheel does.
    #[gpui::test]
    fn a_trackpad_gesture_scrolls_the_instant_it_is_touched(cx: &mut TestAppContext) {
        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, w, cx| {
                scroll_into_history(view, 10);
                let ev = wheel(view, -3., gpui::TouchPhase::Started);
                view.on_scroll(&ev, w, cx);
                assert_eq!(display_offset(view), 7, "the gesture was held back");
                assert!(
                    view.scroll_anim.is_none(),
                    "a trackpad started an animation"
                );
            })
            .unwrap();
    }

    /// Lifting the fingers does not end the stream: macOS keeps sending Moved
    /// events for the momentum tail, and they are *larger* than the gesture
    /// that spawned them. Treating those as a wheel would smooth what the
    /// system is already smoothing.
    #[gpui::test]
    fn a_momentum_tail_is_not_mistaken_for_a_wheel(cx: &mut TestAppContext) {
        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, w, cx| {
                scroll_into_history(view, 10);
                for (lines, phase) in [
                    (-0.5, gpui::TouchPhase::Started),
                    (-1.2, gpui::TouchPhase::Moved),
                    (0., gpui::TouchPhase::Ended),
                    (-2.9, gpui::TouchPhase::Moved),
                    (-2.4, gpui::TouchPhase::Moved),
                ] {
                    let ev = wheel(view, lines, phase);
                    view.on_scroll(&ev, w, cx);
                    assert!(
                        view.scroll_anim.is_none(),
                        "the momentum tail was animated at {lines} lines"
                    );
                }
                assert_eq!(display_offset(view), 3, "the tail did not all land");
            })
            .unwrap();
    }

    /// Inching the wheel one detent at a time reads as continuous already.
    #[gpui::test]
    fn a_scroll_too_small_to_see_jump_stays_direct(cx: &mut TestAppContext) {
        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, w, cx| {
                scroll_into_history(view, 10);
                let ev = notch(view, -0.57);
                view.on_scroll(&ev, w, cx);
                assert!(view.scroll_anim.is_none(), "half a line was animated");
                assert!(view.scroll_frac > 0., "and it did not move either");
            })
            .unwrap();
    }

    #[gpui::test]
    fn turning_smooth_scrolling_off_restores_the_direct_path(cx: &mut TestAppContext) {
        let (window, _daemon) = harness(cx);
        cx.update(|cx| cx.global_mut::<Config>().smooth_scroll = false);
        window
            .update(cx, |view, w, cx| {
                scroll_into_history(view, 10);
                let ev = notch(view, -3.);
                view.on_scroll(&ev, w, cx);
                assert_eq!(display_offset(view), 7);
                assert!(view.scroll_anim.is_none());
            })
            .unwrap();
    }

    /// An animation that kept walking after the view was moved out from under
    /// it would drag the user back off the prompt they just jumped to.
    #[gpui::test]
    fn moving_the_viewport_cancels_an_animation_in_flight(cx: &mut TestAppContext) {
        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, w, cx| {
                scroll_into_history(view, 10);
                let ev = notch(view, -4.9);
                view.on_scroll(&ev, w, cx);
                assert!(view.scroll_anim.is_some());

                view.jump_to_prompt();
                assert!(
                    view.scroll_anim.is_none(),
                    "the animation outlived the jump"
                );
                assert_eq!(display_offset(view), 0);
            })
            .unwrap();
    }

    /// A frame callback that was already queued when the animation was
    /// cancelled must not walk the viewport on its stale epoch.
    #[gpui::test]
    fn a_stale_frame_after_cancellation_does_not_walk(cx: &mut TestAppContext) {
        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, w, cx| {
                scroll_into_history(view, 10);
                let ev = notch(view, -4.9);
                view.on_scroll(&ev, w, cx);
                let epoch = view.scroll_anim_epoch;
                view.cancel_scroll_anim();

                assert!(
                    !view.scroll_anim_frame(epoch, cx),
                    "a stale frame callback kept the animation alive"
                );
                assert_eq!(display_offset(view), 10, "the stale frame moved");
            })
            .unwrap();
    }

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

    #[gpui::test]
    fn ctrl_p_and_ctrl_n_walk_the_history(cx: &mut TestAppContext) {
        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, _, cx| {
                view.history = ["git status", "cargo build", "echo hello"]
                    .into_iter()
                    .map(String::from)
                    .collect();

                view.handle_editor_key(&key("ctrl-p"), cx);
                assert_eq!(view.cmd.text(), "echo hello");
                view.handle_editor_key(&key("ctrl-p"), cx);
                assert_eq!(view.cmd.text(), "cargo build");
                view.handle_editor_key(&key("ctrl-n"), cx);
                assert_eq!(view.cmd.text(), "echo hello");
                view.handle_editor_key(&key("ctrl-n"), cx);
                assert_eq!(view.cmd.text(), "");
            })
            .unwrap();
    }

    #[gpui::test]
    fn ctrl_e_accepts_a_ghost_suggestion_at_the_end(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let (window, mut daemon) = harness(cx);
        prompt_ready(&window, cx, &mut daemon);

        window
            .update(cx, |view, _, cx| {
                assert!(view.input_active(), "the local editor owns a fresh prompt");
                view.history_ranked = vec!["git log --oneline".to_string()];
                view.cmd.set("git l");

                view.handle_editor_key(&key("ctrl-e"), cx);

                assert_eq!(view.cmd.text(), "git log --oneline");
                assert!(
                    view.editor_handoff.is_none(),
                    "accepting a local suggestion must not hand input to the shell"
                );
            })
            .unwrap();
        assert_eq!(
            next_input_until_timeout(&mut daemon),
            None,
            "the local editor must consume Ctrl+E instead of forwarding 0x05"
        );
    }

    #[gpui::test]
    fn ctrl_e_only_moves_to_the_end_when_no_ghost_is_visible(cx: &mut TestAppContext) {
        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, _, cx| {
                view.history_ranked = vec!["git log --oneline".to_string()];
                view.cmd.set_with_cursor("git l", 2);

                view.handle_editor_key(&key("ctrl-e"), cx);

                assert_eq!(view.cmd.text(), "git l");
                assert_eq!(view.cmd.cursor(), view.cmd.len());
            })
            .unwrap();
    }

    #[gpui::test]
    fn an_unknown_ctrl_chord_goes_to_the_shell_with_the_line(cx: &mut TestAppContext) {
        let (window, mut daemon) = harness(cx);
        window
            .update(cx, |view, _, cx| {
                view.cmd.set("echo hi");
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
                view.handle_editor_key(&meta_dot, cx);
                assert_eq!(view.cmd.text(), "ls status");
                assert_eq!(view.cmd.cursor(), "ls status".chars().count());
            })
            .unwrap();
    }

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

    #[gpui::test]
    fn an_intervening_ime_commit_restarts_the_last_word_walk(cx: &mut TestAppContext) {
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

    fn bind_to_a_disconnected_remote_workspace(
        view: &mut TerminalView,
        cx: &mut Context<TerminalView>,
    ) -> crate::core::session::WorkspaceId {
        use crate::core::session::{
            RemoteRef, RemoteTarget, WindowViews, WorkspaceId, WorkspaceStore,
        };
        use crate::terminal::PaneWorkspace;
        let host = RemoteRef::new(
            RemoteTarget::direct("me", "build-box", 22),
            WorkspaceId::new(),
        );
        let entry = crate::core::session::WindowView::on_remote(host.clone());
        let id = entry.id;
        WorkspaceStore::install_for_test(
            cx,
            WindowViews {
                views: vec![entry],
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

    #[gpui::test]
    fn a_disconnected_remote_pane_keeps_the_line_instead_of_handing_it_to_nowhere(
        cx: &mut TestAppContext,
    ) {
        crate::core::config::pin_test_config_dir();
        let (window, mut daemon) = harness(cx);
        cx.update(|cx| crate::ui::keymap::init(cx));
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
                window.activate_window();
                view.focus_handle.focus(window, cx);
                view.cmd.set("zzqqx");
                bind_to_a_disconnected_remote_workspace(view, cx);
            })
            .unwrap();

        let mut vcx = gpui::VisualTestContext::from_window(window.into(), cx);
        vcx.simulate_keystrokes("tab");

        window
            .update(cx, |view, _, cx| {
                assert_eq!(
                    view.cmd.text(),
                    "zzqqx",
                    "a Tab dispatched through SendTab must not empty the line"
                );
                assert!(
                    view.editor_handoff.is_none(),
                    "nothing was handed off, so the editor keeps the prompt"
                );

                view.submit_command(cx);
                assert_eq!(
                    view.cmd.text(),
                    "zzqqx",
                    "submit_command guards the link too, even though on_key_down already does"
                );
            })
            .unwrap();
        assert_eq!(
            next_input_until_timeout(&mut daemon),
            None,
            "not one byte reached the wire"
        );
    }

    #[gpui::test]
    fn a_tab_on_a_detached_remote_pane_never_asks_for_a_listing(cx: &mut TestAppContext) {
        use std::io::Write as _;
        crate::core::config::pin_test_config_dir();
        let (window, mut daemon) = harness(cx);
        DaemonMsg::Prompt {
            active: true,
            at_prompt: true,
            last_exit: None,
        }
        .encode(&mut daemon)
        .unwrap();
        DaemonMsg::Cwd(std::path::PathBuf::from("/home/me/proj"))
            .encode(&mut daemon)
            .unwrap();
        daemon.flush().unwrap();
        wait_for_input_active(&window, cx);
        for _ in 0..200 {
            if window
                .update(cx, |view, _, _| view.cwd().is_some())
                .unwrap()
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        window
            .update(cx, |view, _, cx| {
                view.cmd.set("ls /home/me/");
                bind_to_a_disconnected_remote_workspace(view, cx);
                assert!(
                    view.remote_ssh_cwd().is_some(),
                    "the pane has to look remote enough to want a listing at all"
                );

                view.tab_pressed(true, cx);
                assert!(
                    !view.remote_completion_inflight,
                    "a Tab must not send an SFTP listing down a link that is not attached"
                );
                assert_eq!(
                    view.cmd.text(),
                    "ls /home/me/",
                    "and the line stays where it was"
                );
            })
            .unwrap();
    }

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
                view.dump_hold(0, cx);
            })
            .unwrap();
        assert_eq!(
            next_input_until_timeout(&mut daemon),
            None,
            "a read-only window must not put one byte of typing on the wire"
        );
    }

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

    #[gpui::test]
    fn a_dropped_link_does_not_claim_the_process_exited(cx: &mut TestAppContext) {
        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, _, cx| {
                bind_to_a_disconnected_remote_workspace(view, cx);
                view.handle_event(AlacEvent::Exit, cx);
                assert_eq!(view.title, "tty7 — disconnected");

                view.set_workspace(None);
                view.handle_event(AlacEvent::Exit, cx);
                assert_eq!(view.title, "tty7 — process exited");
            })
            .unwrap();
    }

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

    #[gpui::test]
    fn a_local_pane_types_exactly_as_it_always_did(cx: &mut TestAppContext) {
        let (window, mut daemon) = harness(cx);
        window
            .update(cx, |view, window, cx| {
                bind_to_a_disconnected_remote_workspace(view, cx);
                view.set_workspace(None);
                assert!(view.accepts_input(cx));
                type_char(view, "z", window, cx);
            })
            .unwrap();
        assert_eq!(next_input(&mut daemon), b"z".to_vec());
    }

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
        assert_ne!(
            read_row(cx, 6),
            "before",
            "the mirror must be reset before the daemon replays onto it"
        );

        let resize = loop {
            match ClientMsg::read(&mut new_daemon).expect("the new socket is live") {
                ClientMsg::Resize(win) => break win,
                _ => continue,
            }
        };
        assert_eq!((resize.cols, resize.rows), (100, 30));

        window
            .update(cx, |view, _, cx| view.send_to_pty(b"after", cx))
            .unwrap();
        assert_eq!(next_input(&mut new_daemon), b"after".to_vec());

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

    #[gpui::test]
    fn buffer_search_honors_case_and_regex_toggles(cx: &mut TestAppContext) {
        let (window, mut daemon) = harness(cx);

        DaemonMsg::Output(b"Hello World\r\nhello world\r\nWORLD wide\r\n".to_vec())
            .encode(&mut daemon)
            .unwrap();

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

                set_query(view, "world", window, cx);
                assert_eq!(view.search.as_ref().unwrap().matches.len(), 3);
                assert!(!view.search_regex_error);

                view.search_case_sensitive = true;
                view.recompute_matches(cx);
                assert_eq!(view.search.as_ref().unwrap().matches.len(), 1);
                view.search_case_sensitive = false;

                set_query(view, "wor.d", window, cx);
                assert_eq!(view.search.as_ref().unwrap().matches.len(), 0);
                view.search_regex = true;
                view.recompute_matches(cx);
                assert_eq!(view.search.as_ref().unwrap().matches.len(), 3);

                view.search_regex = true;
                set_query(view, "(", window, cx);
                assert!(view.search_regex_error);
                assert_eq!(view.search.as_ref().unwrap().matches.len(), 0);
                view.search_regex = false;
                view.recompute_matches(cx);
                assert!(!view.search_regex_error);

                view.close_search(window, cx);
                assert_eq!(view.search_last_query, "(");
                assert!(view.search.is_none());
            })
            .unwrap();
    }

    #[gpui::test]
    fn output_under_an_open_search_bar_is_searched_too(cx: &mut TestAppContext) {
        let (window, view, mut daemon) = rooted_harness(cx);

        fn wait_for(cx: &mut TestAppContext, view: &Entity<TerminalView>, needle: char) {
            for _ in 0..200 {
                let seen = cx.update(|cx| {
                    let v = view.read(cx);
                    let term = v.terminal.term.lock();
                    let grid = term.grid();
                    (0..grid.screen_lines() as i32)
                        .any(|l| (0..grid.columns()).any(|c| grid[Line(l)][Column(c)].c == needle))
                });
                if seen {
                    return;
                }
                cx.run_until_parked();
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            panic!("the pane never printed {needle:?}");
        }

        DaemonMsg::Output(b"world 1\r\n".to_vec())
            .encode(&mut daemon)
            .unwrap();
        wait_for(cx, &view, '1');

        window
            .update(cx, |_, window, cx| {
                view.update(cx, |v, cx| {
                    v.open_search(window, cx);
                    let input = v.search.as_ref().unwrap().input.clone();
                    input.update(cx, |s, cx| s.set_value("world", window, cx));
                    v.recompute_matches(cx);
                    assert_eq!(v.search.as_ref().unwrap().matches.len(), 1);
                });
            })
            .unwrap();

        // A second line arrives while the bar is up. Until it is rescanned the
        // count still says 1, and the one highlight it does draw has slid onto
        // whatever line took the old one's place.
        DaemonMsg::Output(b"world 2\r\n".to_vec())
            .encode(&mut daemon)
            .unwrap();
        wait_for(cx, &view, '2');
        for _ in 0..8 {
            cx.executor()
                .advance_clock(super::super::search::SCAN_DEBOUNCE * 2);
            cx.run_until_parked();
        }

        cx.update(|cx| {
            let v = view.read(cx);
            let search = v.search.as_ref().expect("the bar is still open");
            assert_eq!(
                search.matches.len(),
                2,
                "the line printed under the open bar has to be counted too"
            );
            assert_eq!(
                search.current_index,
                Some(0),
                "and the match the user was standing on is still the one they are on"
            );
            assert!(
                !v.search_scan_armed,
                "the debounce has to settle, not rescan forever"
            );
        });
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

    #[gpui::test]
    fn text_area_size_request_replies_with_the_current_geometry(cx: &mut TestAppContext) {
        let (window, mut daemon) = harness(cx);
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

    #[gpui::test]
    fn daemon_output_reaches_the_grid_through_the_event_pump(cx: &mut TestAppContext) {
        let (window, mut daemon) = harness(cx);

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

        DaemonMsg::Output(b" again".to_vec())
            .encode(&mut daemon)
            .unwrap();
        assert_eq!(wait_for(cx, "hello again"), "hello again");
    }

    #[gpui::test]
    fn copy_on_select_writes_the_clipboard_at_mouse_up(cx: &mut TestAppContext) {
        let (window, mut daemon) = harness(cx);

        DaemonMsg::Output(b"hello world".to_vec())
            .encode(&mut daemon)
            .unwrap();
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

        cx.update(|cx| cx.update_global::<Config, _>(|cfg, _| cfg.copy_on_select = true));
        drag_hello(cx);
        let text = cx.update(|cx| cx.read_from_clipboard().and_then(|item| item.text()));
        assert_eq!(text.as_deref(), Some("hello"));

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

    #[gpui::test]
    fn ctrl_c_copy_consumes_the_selection_so_the_next_press_is_sigint(cx: &mut TestAppContext) {
        let (window, mut daemon) = harness(cx);

        DaemonMsg::Output(b"hello world".to_vec())
            .encode(&mut daemon)
            .unwrap();
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
                view.on_select_start(0, 0, true, 1, false, cx);
                view.on_select_update(4, 0, false, cx);
                view.on_select_end(cx);
                assert!(view.has_selection(), "the drag must leave a selection");

                let consumed = view.handle_cmd_shortcut(&key("ctrl-c"), window, cx);
                assert!(matches!(consumed, CmdKey::Consumed));
                assert!(
                    !view.has_selection(),
                    "the Ctrl+C copy must consume the selection"
                );

                let fell_through = view.handle_cmd_shortcut(&key("ctrl-c"), window, cx);
                assert!(matches!(fell_through, CmdKey::FallThrough));
            })
            .unwrap();
        let text = cx.update(|cx| cx.read_from_clipboard().and_then(|item| item.text()));
        assert_eq!(text.as_deref(), Some("hello"));
    }

    #[cfg(target_os = "macos")]
    #[gpui::test]
    fn cmd_backspace_reaches_a_foreground_tui_as_ctrl_u(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let (window, mut daemon) = harness(cx);
        window
            .update(cx, |view, window, cx| {
                assert!(
                    !view.input_active(),
                    "the foreground process, not tty7's editor, owns input"
                );
                view.on_key_down(
                    &KeyDownEvent {
                        keystroke: gpui::Keystroke {
                            modifiers: Modifiers {
                                platform: true,
                                ..Modifiers::default()
                            },
                            key: "backspace".into(),
                            key_char: None,
                        },
                        is_held: false,
                        prefer_character_input: false,
                    },
                    window,
                    cx,
                );
            })
            .unwrap();

        assert_eq!(next_input_until_timeout(&mut daemon), Some(vec![0x15]));
    }

    #[cfg(target_os = "macos")]
    #[gpui::test]
    fn cmd_navigation_reaches_a_foreground_tui_as_readline_controls(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let (window, mut daemon) = harness(cx);

        for (key, expected) in [("left", 0x01), ("right", 0x05), ("delete", 0x0b)] {
            window
                .update(cx, |view, window, cx| {
                    assert!(!view.input_active(), "the foreground TUI owns input");
                    view.on_key_down(
                        &KeyDownEvent {
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
                        },
                        window,
                        cx,
                    );
                })
                .unwrap();
            assert_eq!(next_input_until_timeout(&mut daemon), Some(vec![expected]));
        }
    }

    #[cfg(target_os = "macos")]
    #[gpui::test]
    fn cmd_backspace_releases_held_input_before_ctrl_u(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let (window, mut daemon) = harness(cx);
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
            .update(cx, |view, window, cx| {
                view.commit_text("ls", cx);
                view.on_key_down(
                    &KeyDownEvent {
                        keystroke: gpui::Keystroke {
                            modifiers: Modifiers {
                                platform: true,
                                ..Modifiers::default()
                            },
                            key: "backspace".into(),
                            key_char: None,
                        },
                        is_held: false,
                        prefer_character_input: false,
                    },
                    window,
                    cx,
                );
            })
            .unwrap();

        assert_eq!(
            next_input_until_timeout(&mut daemon),
            Some(b"ls".to_vec()),
            "held text must reach the PTY before the line-kill"
        );
        assert_eq!(
            next_input_until_timeout(&mut daemon),
            Some(vec![0x15]),
            "Ctrl-U must follow the text it clears"
        );
    }

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
        assert_eq!(next_input(&mut daemon), b"echo hi".to_vec());
    }

    #[gpui::test]
    fn ctrl_c_copy_consumes_the_editor_selection_at_the_prompt(cx: &mut TestAppContext) {
        let (window, mut daemon) = harness(cx);

        DaemonMsg::Prompt {
            active: true,
            at_prompt: true,
            last_exit: Some(0),
        }
        .encode(&mut daemon)
        .unwrap();
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

                let consumed = view.handle_cmd_shortcut(&key("ctrl-c"), window, cx);
                assert!(matches!(consumed, CmdKey::Consumed));
                assert!(
                    view.cmd.selection().is_none(),
                    "the Ctrl+C copy must consume the editor selection"
                );

                let fell_through = view.handle_cmd_shortcut(&key("ctrl-c"), window, cx);
                assert!(matches!(fell_through, CmdKey::FallThrough));
            })
            .unwrap();
        let text = cx.update(|cx| cx.read_from_clipboard().and_then(|item| item.text()));
        assert_eq!(text.as_deref(), Some("echo hi"));
    }

    #[gpui::test]
    fn hidden_cursor_at_prompt_anchors_the_editor_at_the_real_cell_not_top_left(
        cx: &mut TestAppContext,
    ) {
        use alacritty_terminal::vte::ansi::CursorShape;

        let (window, mut daemon) = harness(cx);

        DaemonMsg::Prompt {
            active: true,
            at_prompt: true,
            last_exit: Some(0),
        }
        .encode(&mut daemon)
        .unwrap();
        DaemonMsg::Output(b"\x1b[4;11H\x1b[?25l".to_vec())
            .encode(&mut daemon)
            .unwrap();

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

        let (window, mut daemon) = harness(cx);
        let got = subscribe(&window, cx);
        DaemonMsg::Exited { code: Some(0) }
            .encode(&mut daemon)
            .unwrap();
        wait_exited(&window, cx);
        assert!(got.get(), "a genuine child exit must emit ChildExited");

        let (window, daemon) = harness(cx);
        let got = subscribe(&window, cx);
        drop(daemon);
        wait_exited(&window, cx);
        assert!(!got.get(), "a daemon disconnect must not emit ChildExited");
    }

    #[gpui::test]
    fn ssh_drop_mid_tui_recovers_at_the_next_prompt(cx: &mut TestAppContext) {
        use alacritty_terminal::vte::ansi::CursorShape;

        let (window, mut daemon) = harness(cx);

        DaemonMsg::Output(b"\x1b[?1049h\x1b[?25l".to_vec())
            .encode(&mut daemon)
            .unwrap();
        DaemonMsg::Prompt {
            active: true,
            at_prompt: true,
            last_exit: Some(255),
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

        window
            .update(cx, |view, _, _| {
                assert!(
                    view.input_active(),
                    "off the alt screen and at the prompt, the editor is live"
                );
            })
            .unwrap();
    }

    #[gpui::test]
    fn generator_results_merge_into_the_open_menu(cx: &mut TestAppContext) {
        use crate::terminal::generator::Parsed;

        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, _, cx| {
                view.cmd.set_with_cursor("git checkout ma", 15);
                let session = CompletionSession::new(13, String::new(), Vec::new(), 1);
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
                assert_eq!(shown, vec!["main", "mainline"]);
                assert_eq!(s.selected().unwrap().text, "main");
            })
            .unwrap();
    }

    #[gpui::test]
    fn generator_result_for_a_closed_menu_is_dropped(cx: &mut TestAppContext) {
        use crate::terminal::generator::Parsed;

        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, _, cx| {
                view.cmd.set_with_cursor("git checkout ", 13);
                let session = CompletionSession::new(13, String::new(), Vec::new(), 1);
                let stale = view.open_completion(session);
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

                let fresh =
                    view.open_completion(CompletionSession::new(13, String::new(), Vec::new(), 1));
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

    #[gpui::test]
    fn a_remote_workspace_pane_reports_its_cwd_as_remote(cx: &mut TestAppContext) {
        use std::io::Write as _;
        let (window, mut daemon) = harness(cx);
        DaemonMsg::Cwd(std::path::PathBuf::from("/home/me/proj"))
            .encode(&mut daemon)
            .unwrap();
        daemon.flush().unwrap();
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
                assert_eq!(
                    view.local_cwd(),
                    Some(std::path::PathBuf::from("/home/me/proj"))
                );
                assert_eq!(view.remote_ssh_cwd(), None);

                bind_to_a_disconnected_remote_workspace(view, cx);

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

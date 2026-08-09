use gpui::{
    Animation, AnimationExt as _, AnyElement, App, Axis, Bounds, Context, FontWeight, MouseButton,
    MouseDownEvent, Pixels, SharedString, Window, canvas, deferred, div, ease_out_quint,
    linear_color_stop, linear_gradient, prelude::*, px,
};
use gpui_component::button::{Button, ButtonCustomVariant, ButtonVariants as _};
use gpui_component::input::Input;
use gpui_component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_component::{ActiveTheme as _, Icon, IconName, Selectable as _, Sizable as _, h_flex};
use std::cell::RefCell;
use std::rc::Rc;

use crate::core::actions::{
    CloseActiveTab, CloseOtherTabs, CloseTabsToTheRight, CopyAgentSessionId, CopyWorkingDirectory,
    ForkAgentSession, MarkTabUnread, NewWorktreeTab, OpenSettings, RenameTab, SelectWorkspace1,
    SelectWorkspace2, SelectWorkspace3, SelectWorkspace4, SelectWorkspace5, SelectWorkspace6,
    SelectWorkspace7, SelectWorkspace8, SelectWorkspace9, SplitDown, SplitRight, TogglePalette,
};
use crate::core::config::RightPanelTab;
use crate::core::shells::DetectedShell;
use crate::daemon::protocol::ShellSpec;
use crate::ui::app::{TILE_GLYPH, TILE_GLYPH_LINE, TILE_SIZE, Tab, Tty7App, tile_trailing_inset};
use crate::ui::hints::tab_badge_label;
use crate::ui::i18n::{L10nKey, t, t_fmt};
use crate::ui::reorder::{self, Reorder, Surface};

/// One duration and one curve for every transition the app runs, so a fade and
/// a slide read as the same hand. Long enough to be seen as movement, short
/// enough that nobody waits on it.
pub(crate) const TRANSITION_MS: u64 = 140;
pub(crate) const REORDER_SLIDE_MS: u64 = TRANSITION_MS;
const CHIP_GAP: f32 = 6.;

pub(crate) const GRAB_HANDLE_W: f32 = 80.;

const KEEP_SEGMENTS: usize = 3;

/// Builds a launch specification without recomputing argument ownership locally.
/// The inventory may originate from a remote host, so only its transported
/// metadata can distinguish tty7 launch defaults from user-authored arguments.
fn shell_spec(shell: &DetectedShell) -> ShellSpec {
    ShellSpec {
        program: shell.program.clone(),
        args: shell.args.clone(),
        args_are_tty7_defaults: shell.args_are_tty7_defaults,
    }
}

/// Strips a `user@host:` prefix a shell put in front of its title, leaving
/// the path (or command) it actually names. A bare `host:` with no user is
/// left alone — that is a drive letter on Windows.
pub(crate) fn strip_host_prefix(raw: &str) -> &str {
    match raw.split_once(':') {
        Some((head, tail)) if head.contains('@') => tail,
        _ => raw,
    }
}

pub(crate) fn abbreviate_home(path: &str) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    if path.starts_with('~') {
        return Cow::Borrowed(path);
    }
    let Some(home) = std::env::var_os("HOME") else {
        return Cow::Borrowed(path);
    };
    let home = home.to_string_lossy();
    let home = home.trim_end_matches('/');
    if home.is_empty() {
        return Cow::Borrowed(path);
    }
    if path == home {
        return Cow::Owned("~".to_string());
    }
    match path.strip_prefix(home) {
        Some(rest) if rest.starts_with('/') => Cow::Owned(format!("~{rest}")),
        _ => Cow::Borrowed(path),
    }
}

/// The separator a path spells itself with. A path carrying a single `\` is
/// a Windows path and has to be put back together with `\`: rejoining it with
/// `/` would make one tab spell its location two ways, `C:\Users\dev\app`
/// while it fits and `C:/…/app` once it has to be elided.
fn path_separator(path: &str) -> char {
    if path.contains('\\') { '\\' } else { '/' }
}

fn join_segments(segments: &[&str], sep: char) -> String {
    segments.join(sep.encode_utf8(&mut [0u8; 4]) as &str)
}

pub(crate) fn short_title(raw: &str) -> String {
    let raw = raw.trim();
    if raw.is_empty() {
        return String::new();
    }
    let after_host = strip_host_prefix(raw);
    let after_host = after_host.trim();
    if after_host.is_empty() {
        return String::new();
    }
    let abbreviated = abbreviate_home(after_host);
    let path: &str = abbreviated.as_ref();

    enum Kind {
        Home,
        Absolute,
        Relative,
    }
    let (kind, body) = if let Some(rest) = path.strip_prefix("~/") {
        (Kind::Home, rest)
    } else if path == "~" {
        return "~".to_string();
    } else if let Some(rest) = path.strip_prefix('/') {
        (Kind::Absolute, rest)
    } else {
        (Kind::Relative, path)
    };

    // Both separators: Windows shells report `C:\Users\…` while git and the
    // terminal integration use `/`, and a path must be cut on either one.
    let segments: Vec<&str> = body.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
    if segments.is_empty() {
        return match kind {
            Kind::Home => "~",
            Kind::Absolute => "/",
            Kind::Relative => "",
        }
        .to_string();
    }

    let sep = path_separator(path);
    let depth = segments.len() + usize::from(matches!(kind, Kind::Home));
    let mut label = if depth > KEEP_SEGMENTS {
        let tail = &segments[segments.len() - KEEP_SEGMENTS..];
        format!("…{sep}{}", join_segments(tail, sep))
    } else {
        match kind {
            Kind::Home => format!("~{sep}{}", join_segments(&segments, sep)),
            Kind::Absolute => format!("/{}", join_segments(&segments, sep)),
            Kind::Relative => join_segments(&segments, sep),
        }
    };
    if label.chars().count() > 40 {
        label = format!("{}…", label.chars().take(40).collect::<String>());
    }
    label
}

/// Width of `text` shaped in `font` at `size`, in pixels.
///
/// The window's text system caches shaped runs, so measuring the same labels
/// across frames is cheap. The sidebar elides against real glyph widths
/// instead of guessing at character counts — that is the only way a mixed
/// CJK/Latin label can be squeezed without tearing mid-token.
pub(crate) fn measure_text(
    text_system: &gpui::WindowTextSystem,
    font: &gpui::Font,
    size: f32,
    text: &str,
) -> f32 {
    text_system
        .shape_line(
            SharedString::from(text),
            px(size),
            &[gpui::TextRun {
                len: text.len(),
                font: font.clone(),
                color: gpui::Hsla::default(),
                background_color: None,
                underline: None,
                strikethrough: None,
            }],
            None,
        )
        .width
        .as_f32()
}

/// Elides a path from the front when it cannot fit `max_width`, keeping the
/// root marker (drive letter, `~`, or the leading slash) and every trailing
/// segment that fits.
///
/// The tail is what a user identifies a tab by — the file or directory they
/// are actually working on — so it is never torn: whole segments drop off the
/// front first (a half-eaten directory name reads as noise), and when even
/// the last segment is too wide, only that segment is elided character by
/// character, still tail-first.
pub(crate) fn elide_path_keep_tail(
    text_system: &gpui::WindowTextSystem,
    font: &gpui::Font,
    size: f32,
    path: &str,
    max_width: f32,
) -> SharedString {
    let path = path.trim();
    if path.is_empty() || measure_text(text_system, font, size, path) <= max_width {
        return SharedString::from(path);
    }
    let sep = path_separator(path);
    let segments: Vec<&str> = path.split(['/', '\\']).collect();
    // A leading slash splits into an empty first segment; `~` and drive
    // letters (`E:`) carry the same "where this tree lives" weight, and a
    // leading `…` means `short_title` already elided once — that marker is
    // replaced by the new elision instead of stacking two ellipses. Keep
    // whichever marker there is so the result never reads as a bare
    // relative path.
    let root: &str = match segments.first() {
        Some(&"") => "/",
        Some(&"~") => "~",
        Some(&"…") => "",
        Some(head) if head.ends_with(':') => head,
        _ => "",
    };
    let root_kept = segments
        .first()
        .is_some_and(|s| s.is_empty() || *s == "~" || *s == "…" || s.ends_with(':'));
    let prefix = if root.is_empty() {
        format!("…{sep}")
    } else if root == "/" {
        // The absolute-path root is already the slash itself.
        format!("/…{sep}")
    } else {
        format!("{root}{sep}…{sep}")
    };
    // Drop whole segments from the front until the remaining tail fits. The
    // width only shrinks as segments leave, so the first fit is the widest
    // one — greedy is optimal here.
    //
    // With a root marker, `head = 1` would spell the root, the ellipsis, and
    // then every remaining segment — strictly wider than the original that
    // already failed to fit — so that candidate is skipped rather than
    // measured.
    let mut head = if root_kept { 2 } else { 0 };
    while head < segments.len() {
        let candidate = if head == 0 {
            join_segments(&segments, sep)
        } else {
            format!("{prefix}{}", join_segments(&segments[head..], sep))
        };
        if measure_text(text_system, font, size, &candidate) <= max_width {
            return SharedString::from(candidate);
        }
        if head + 1 >= segments.len() {
            break;
        }
        head += 1;
    }
    // Even the last segment alone is too wide: keep its tail after the
    // ellipsis, with no slash so the reader sees the segment was torn.
    elide_tail_chars(
        text_system,
        font,
        size,
        segments[segments.len() - 1],
        max_width,
    )
}

/// Characters a token is allowed to break on. Space is in the set because
/// this also elides labels a human typed — `Backend server logs` — not just
/// branch names, and a word boundary is the cut a reader forgives.
const TOKEN_BREAKS: [char; 5] = ['-', '_', '/', '.', ' '];

/// The head this token would rather keep: six glyphs, extended to just past
/// the next break so the cut lands on a boundary (`window-…` rather than
/// `window…`). When no break is within reach the plain six is kept — running
/// on to the cap would spend the whole budget on a prefix and leave the tail,
/// which is what identifies the token, with nothing.
fn preferred_head(chars: &[char]) -> usize {
    let base = chars.len().min(6);
    let cap = chars.len().min(12);
    if base >= cap {
        return base;
    }
    match chars[base..cap]
        .iter()
        .position(|c| TOKEN_BREAKS.contains(c))
    {
        Some(offset) => base + offset + 1,
        None => base,
    }
}

/// Elides the middle of a single token (a branch name, a shell name, a label
/// the user typed) so both its head and its identifying tail survive:
/// `window-transparency-backdrop` reads `window-…backdrop` in a narrow
/// sidebar instead of losing its tail to a trailing ellipsis.
///
/// A head that fits but leaves no room for a tail is worse than no head at
/// all, so the preferred head is given up for a shorter one when that is what
/// it takes to keep a few trailing glyphs; only when even a three-glyph head
/// cannot buy a tail does this fall back to a tail-only elision.
pub(crate) fn elide_keep_edges(
    text_system: &gpui::WindowTextSystem,
    font: &gpui::Font,
    size: f32,
    text: &str,
    max_width: f32,
) -> SharedString {
    let text = text.trim();
    if text.is_empty() || measure_text(text_system, font, size, text) <= max_width {
        return SharedString::from(text);
    }
    let chars: Vec<char> = text.chars().collect();
    let shaped = |head_n: usize, tail_n: usize| -> f32 {
        let mut s: String = chars[..head_n].iter().collect();
        s.push('…');
        s.extend(chars[chars.len() - tail_n..].iter());
        measure_text(text_system, font, size, &s)
    };
    // Width is monotone in the tail length, so a binary search finds the
    // longest tail that still fits behind a given head.
    let longest_tail = |head_n: usize| -> usize {
        let (mut lo, mut hi) = (0usize, chars.len() - head_n);
        while lo < hi {
            let mid = (lo + hi + 1) / 2;
            if shaped(head_n, mid) <= max_width {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        lo
    };
    /// Enough trailing glyphs to tell two sibling branches apart.
    const MIN_TAIL: usize = 3;
    let preferred = preferred_head(&chars);
    let mut candidates = vec![preferred, 6, 3];
    candidates.retain(|&h| h > 0 && h <= chars.len());
    candidates.dedup();
    let mut best: Option<(usize, usize)> = None;
    for head_n in candidates {
        if shaped(head_n, 0) > max_width {
            continue;
        }
        let tail = longest_tail(head_n);
        if tail >= MIN_TAIL.min(chars.len() - head_n) {
            best = Some((head_n, tail));
            break;
        }
        if best.is_none_or(|(_, best_tail)| tail > best_tail) {
            best = Some((head_n, tail));
        }
    }
    let Some((head, tail)) = best.filter(|&(_, tail)| tail > 0) else {
        // No head buys a tail worth showing; a bare tail says more.
        return elide_tail_chars(text_system, font, size, text, max_width);
    };
    let mut out = String::with_capacity(head + 1 + tail);
    out.extend(chars[..head].iter());
    out.push('…');
    out.extend(chars[chars.len() - tail..].iter());
    SharedString::from(out)
}

/// Elides a row label. A path keeps its tail — the file or directory being
/// worked on — while anything else keeps both edges.
///
/// A shell title is not always a path: `npm run dev`, `man git-log`, or a name
/// the user typed into the rename box. Running those through the path rule
/// drops their head, which is the part that names them, and `… server logs`
/// says less than the CSS truncation this replaced.
pub(crate) fn elide_label(
    text_system: &gpui::WindowTextSystem,
    font: &gpui::Font,
    size: f32,
    text: &str,
    max_width: f32,
) -> SharedString {
    if text.contains('/') || text.contains('\\') {
        elide_path_keep_tail(text_system, font, size, text, max_width)
    } else {
        elide_keep_edges(text_system, font, size, text, max_width)
    }
}

/// Keeps the longest tail of `text` that fits after a bare ellipsis. Shared
/// by the path and token elisions as their last resort.
fn elide_tail_chars(
    text_system: &gpui::WindowTextSystem,
    font: &gpui::Font,
    size: f32,
    text: &str,
    max_width: f32,
) -> SharedString {
    let budget = max_width - measure_text(text_system, font, size, "…");
    if budget <= 0. {
        return SharedString::from("…");
    }
    let chars: Vec<char> = text.chars().collect();
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let mid = (lo + hi + 1) / 2;
        let s: String = chars[chars.len() - mid..].iter().collect();
        if measure_text(text_system, font, size, &s) <= budget {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    if lo == 0 {
        return SharedString::from("…");
    }
    let mut out = String::with_capacity(1 + lo);
    out.push('…');
    out.extend(chars[chars.len() - lo..].iter());
    SharedString::from(out)
}

#[derive(Clone)]
pub(crate) struct DragTab;

impl Render for DragTab {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// What a chrome tile says on hover: what it does, then the chord that does it.
/// The tile's own name is no use as a tooltip — the workspace head already
/// wears it as its label.
pub(crate) fn chord_hint(what: &str, action: &str, cx: &gpui::App) -> SharedString {
    match crate::ui::home::key_hint(action, cx) {
        Some(keys) => SharedString::from(format!("{what}  {keys}")),
        None => SharedString::from(what.to_string()),
    }
}

pub(crate) fn chrome_tile_variant(cx: &gpui::App) -> ButtonCustomVariant {
    chrome_tile_variant_for(false, cx)
}

pub(crate) fn chrome_tile_variant_for(selected: bool, cx: &gpui::App) -> ButtonCustomVariant {
    ButtonCustomVariant::new(cx)
        .color(cx.theme().transparent)
        .foreground(if selected {
            cx.theme().foreground
        } else {
            cx.theme().sidebar_foreground
        })
        // `sidebar_accent` is the surface's *selected* step, and it was handed
        // to hover as well — so a hovered tile wore the fill of a selected one
        // and, with the right panel open, two tiles read as current at once.
        // Hover takes the step the palette derives for it. (A selected button
        // never renders the hover style, so this only reaches the rest.)
        .hover(gpui::rgb(cx.global::<crate::ui::presets::Surfaces>().sidebar.hover).into())
        .active(cx.theme().sidebar_accent)
}

pub(crate) const BUTTON_ICON_SCALE: f32 = 0.75;

/// WCAG 2.2 SC 2.5.8 puts the desktop floor for a pointer target at 24×24, and
/// gpui-component renders an icon-only `.xsmall()` button as a 20×20 box (18×18
/// where the chrome overrode it). Grow only the box: the glyph keeps its size,
/// so the chrome looks unchanged and simply stops being fiddly to hit.
pub(crate) const MIN_TARGET: f32 = 24.;

pub(crate) fn hit_target(button: Button) -> Button {
    button.w(px(MIN_TARGET)).h(px(MIN_TARGET))
}

/// The narrowest a chip gets: its `min_w`, which flex-shrink cannot go under.
const CHIP_MIN_W: f32 = 100.;

/// The run of chips to draw when they cannot all fit.
///
/// The row clips what overflows, so past a certain tab count the chips at the
/// end simply were not drawn — including, right after ⌘T, the tab that was
/// just opened and made active. Slide the run instead: keep it anchored at the
/// first tab until the active one would fall off the right edge, then move it
/// by as little as it takes to hold the active chip.
fn visible_chips(order: &[usize], active: usize, avail: f32) -> Vec<usize> {
    let fits = ((avail / (CHIP_MIN_W + CHIP_GAP)).floor() as usize).max(1);
    if order.len() <= fits {
        return order.to_vec();
    }
    let at = order.iter().position(|&i| i == active).unwrap_or(0);
    let start = at.saturating_sub(fits - 1).min(order.len() - fits);
    order[start..start + fits].to_vec()
}

pub(crate) fn chrome_tile(button: Button, selected: bool, cx: &gpui::App) -> Button {
    chrome_tile_sized(button, TILE_SIZE, TILE_GLYPH, selected, cx)
}

pub(crate) fn chrome_tile_sized(
    button: Button,
    tile: f32,
    glyph: f32,
    selected: bool,
    cx: &gpui::App,
) -> Button {
    button
        .custom(chrome_tile_variant_for(selected, cx))
        .selected(selected)
        .with_size(px(glyph / BUTTON_ICON_SCALE))
        .w(px(tile))
        .h(px(tile))
}

/// The words behind the status dot's colour.
pub(crate) fn agent_status_label(
    status: Option<crate::core::cli_agent::AgentStatus>,
) -> Option<&'static str> {
    use crate::core::cli_agent::AgentStatus;
    match status? {
        AgentStatus::Idle => None,
        AgentStatus::Working => Some(t(L10nKey::AgentStatusWorking)),
        AgentStatus::Waiting => Some(t(L10nKey::AgentStatusWaiting)),
        AgentStatus::Done => Some(t(L10nKey::AgentStatusDone)),
    }
}

pub(crate) const LIVE_DOT: u32 = 0x22C55E;

pub(crate) const UNKNOWN_DOT: u32 = 0x9AA0A6;

pub(crate) fn workspace_avatar(
    name: &str,
    live: crate::terminal::pane_liveness::Liveness,
    current: bool,
    size: f32,
    cx: &App,
) -> impl IntoElement + use<> {
    use crate::terminal::pane_liveness::Liveness;
    let dot = match live {
        Liveness::Alive => Some(LIVE_DOT),
        Liveness::Unknown => Some(UNKNOWN_DOT),
        Liveness::Stopped => None,
    };
    let initial: String = name
        .chars()
        .next()
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_else(|| "~".to_string());
    div()
        .relative()
        .flex_shrink_0()
        .size(px(size))
        .child(
            div()
                .size(px(size))
                .rounded_full()
                .bg(cx.theme().secondary)
                .flex()
                .items_center()
                .justify_center()
                .text_size(px((size * 0.46).round()))
                .font_weight(FontWeight::MEDIUM)
                .text_color(cx.theme().foreground.opacity(0.65))
                .child(initial)
                .when(!current, |disc| disc.opacity(0.55)),
        )
        .children(dot.map(|rgb| Tty7App::status_dot(rgb, 0, size, cx.theme().popover, false)))
}

pub(crate) fn select_workspace_action(index: usize) -> Option<Box<dyn gpui::Action>> {
    Some(match index {
        0 => Box::new(SelectWorkspace1) as Box<dyn gpui::Action>,
        1 => Box::new(SelectWorkspace2),
        2 => Box::new(SelectWorkspace3),
        3 => Box::new(SelectWorkspace4),
        4 => Box::new(SelectWorkspace5),
        5 => Box::new(SelectWorkspace6),
        6 => Box::new(SelectWorkspace7),
        7 => Box::new(SelectWorkspace8),
        8 => Box::new(SelectWorkspace9),
        _ => return None,
    })
}

impl Tty7App {
    pub(crate) const AVATAR_PX: f32 = 20.0;

    pub(crate) fn workspace_head(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        if let Some(rename) = self.workspace_rename.as_ref() {
            return h_flex()
                .id("workspace-rename")
                .flex_shrink_0()
                .items_center()
                .h(px(30.))
                .w_full()
                .px(px(7.))
                .rounded_md()
                .bg(cx.theme().sidebar_accent)
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(Input::new(&rename.input).appearance(false).xsmall())
                .into_any_element();
        }

        crate::terminal::pane_liveness::sweep(cx);
        let current = crate::ui::machine_mirror::display_name_for(cx, self.workspace)
            .unwrap_or_else(|| "tty7".to_string());
        let monogram: String = current
            .chars()
            .next()
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or_else(|| "~".to_string());

        div()
            .occlude()
            .w_full()
            .capture_any_mouse_down(|ev: &gpui::MouseDownEvent, _window, cx| {
                if ev.button == MouseButton::Right {
                    cx.stop_propagation();
                }
            })
            .child(
                Button::new("rail-workspace-head")
                    .custom(chrome_tile_variant(cx))
                    .child(
                        h_flex()
                            .w_full()
                            .items_center()
                            .gap(px(6.))
                            .child(
                                div()
                                    .flex()
                                    .flex_shrink_0()
                                    .items_center()
                                    .justify_center()
                                    .size(px(Self::AVATAR_PX))
                                    .rounded_full()
                                    .bg(cx.theme().secondary)
                                    .text_size(px(10.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(monogram),
                            )
                            .child(
                                div()
                                    .flex_shrink(1.)
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(12.5))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(SharedString::from(current.clone())),
                            )
                            .child(
                                // Not a chevron-down: this opens a centred
                                // panel, not a menu hanging off the button.
                                Icon::empty()
                                    .path("icons/chevrons-up-down.svg")
                                    .size(px(11.))
                                    .flex_shrink_0()
                                    .text_color(cx.theme().muted_foreground),
                            ),
                    )
                    .xsmall()
                    .w_full()
                    .h(px(30.))
                    .rounded_md()
                    .tooltip(chord_hint(
                        t(L10nKey::HomeSwitchWorkspace),
                        "ToggleSwitcher",
                        cx,
                    ))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.toggle_switcher(window, cx);
                    })),
            )
            .into_any_element()
    }

    pub(crate) fn app_menu_tile(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let action_ctx = self
            .tabs
            .get(self.active)
            .and_then(|t| t.pane.focused_or_first(window, cx))
            .map(|leaf| leaf.read(cx).focus_handle.clone())
            .unwrap_or_else(|| self.home_focus.clone());
        div().occlude().flex_shrink_0().child(
            chrome_tile(
                Button::new("titlebar-app-menu").icon(IconName::Ellipsis),
                false,
                cx,
            )
            .rounded_lg()
            .tooltip(t(L10nKey::TabTooltipMore))
            .dropdown_menu_with_anchor(
                gpui::Anchor::TopRight,
                move |menu, _window, _cx| {
                    menu.min_w(px(200.))
                        .action_context(action_ctx.clone())
                        .menu(t(L10nKey::AppMenuCommandPalette), Box::new(TogglePalette))
                        .menu(t(L10nKey::AppMenuSettings), Box::new(OpenSettings))
                },
            ),
        )
    }

    pub(crate) fn window_chrome(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let panel_open = self.right_panel_open(cx);
        h_flex()
            .flex_shrink_0()
            .items_center()
            .gap(px(2.))
            .pr(px(tile_trailing_inset()))
            .when(!cfg!(target_os = "macos"), |this| this.pr_1())
            .child(
                div().occlude().flex_shrink_0().child(
                    chrome_tile(
                        Button::new("titlebar-right-panel")
                            .icon(Icon::empty().path("icons/panel-right.svg")),
                        false,
                        cx,
                    )
                    .rounded_lg()
                    .tooltip(chord_hint(
                        match panel_open {
                            true => t(L10nKey::TabTooltipHideDetailPanel),
                            false => t(L10nKey::TabTooltipShowDetailPanel),
                        },
                        "ToggleRightPanel",
                        cx,
                    ))
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.toggle_right_panel(cx);
                    })),
                ),
            )
            .child(self.app_menu_tile(window, cx))
    }

    pub(crate) fn right_panel_tabs(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let active_tab = self.right_panel_tab;
        let changed = match &self.right_panel.diff {
            Some(Some(snap)) => {
                let n = snap.files.len() + snap.untracked_count();
                (n > 0).then_some(n)
            }
            _ => None,
        };
        [
            (
                RightPanelTab::Info,
                Icon::empty().path("icons/info.svg"),
                L10nKey::PanelInfoTitle,
            ),
            (
                RightPanelTab::Changes,
                Icon::empty().path("icons/git-branch.svg"),
                L10nKey::PanelChangesTitle,
            ),
            (
                RightPanelTab::Files,
                Icon::new(IconName::FolderClosed),
                L10nKey::PanelFilesTitle,
            ),
        ]
        .into_iter()
        .map(|(tab, icon, label_key)| {
            div()
                .occlude()
                .flex_shrink_0()
                .child(
                    chrome_tile(
                        Button::new(("right-panel-tab", tab as usize)).icon(icon),
                        active_tab == tab,
                        cx,
                    )
                    .rounded_lg()
                    .tooltip(match (tab, changed) {
                        (RightPanelTab::Changes, Some(n)) => {
                            SharedString::from(format!("{} · {n}", t(label_key)))
                        }
                        _ => SharedString::from(t(label_key)),
                    })
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        this.set_right_panel_tab(tab, cx);
                    })),
                )
                .into_any_element()
        })
        .collect()
    }

    /// Working and Done differ only in hue (blue vs green), and Waiting vs Done
    /// — the pair that actually decides whether you go and look — is amber vs
    /// green, the pair red-green colour vision separates worst. Give Waiting a
    /// hole so it is a different *shape*, not just a different colour.
    fn status_dot(
        rgb: u32,
        unread: usize,
        size: f32,
        ring: gpui::Hsla,
        hollow: bool,
    ) -> gpui::AnyElement {
        let d = (size * 0.42).max(7.);
        let bg = ring;
        if unread > 0 {
            let nd = (size * 0.72).max(13.0);
            let label = unread.min(9).to_string();
            div()
                .absolute()
                .right(px(-(nd - d) / 2.0 - d * 0.22))
                .bottom(px(-(nd - d) / 2.0 - d * 0.22))
                .size(px(nd))
                .rounded_full()
                .border_1()
                .border_color(bg)
                .bg(gpui::rgb(rgb))
                .flex()
                .items_center()
                .justify_center()
                .text_size(px((nd * 0.62).round()))
                .font_weight(FontWeight::BOLD)
                .text_color(gpui::white())
                .child(label)
                .into_any_element()
        } else {
            div()
                .absolute()
                .right(px(-(d * 0.22)))
                .bottom(px(-(d * 0.22)))
                .size(px(d))
                .rounded_full()
                .border_2()
                .border_color(bg)
                .bg(gpui::rgb(rgb))
                .when(hollow, |dot| {
                    dot.flex()
                        .items_center()
                        .justify_center()
                        .child(div().size(px((d * 0.36).max(2.5))).rounded_full().bg(bg))
                })
                .into_any_element()
        }
    }

    pub(crate) fn tab_avatar(
        &self,
        id: impl Into<gpui::ElementId>,
        agent: Option<crate::core::cli_agent::CLIAgent>,
        status: Option<crate::core::cli_agent::AgentStatus>,
        unread: usize,
        ssh: Option<u32>,
        size: f32,
        cx: &App,
    ) -> gpui::AnyElement {
        let base = div()
            .id(id)
            .flex_shrink_0()
            .size(px(size))
            .flex()
            .items_center()
            .justify_center();
        match agent {
            Some(agent) => {
                let hollow = status == Some(crate::core::cli_agent::AgentStatus::Waiting);
                let dot = status
                    .and_then(|s| s.dot_rgb())
                    .map(|rgb| Self::status_dot(rgb, unread, size, cx.theme().background, hollow));
                // Which agent this is, and what it wants, were carried entirely
                // by a brand hue and a nine-pixel dot. Say it in words too.
                let tip = match agent_status_label(status) {
                    Some(state) => format!("{} — {state}", agent.display_name()),
                    None => agent.display_name().to_string(),
                };
                base.relative()
                    .rounded_full()
                    .bg(gpui::rgb(agent.accent_rgb()))
                    // Codex and Grok are both pure black, which is the window
                    // fill on a dark theme — the disc dissolves and leaves the
                    // glyph floating. A hairline keeps it a disc in any theme.
                    .when(
                        crate::ui::presets::needs_edge(agent.accent_rgb(), cx.theme().background),
                        |d| d.border_1().border_color(cx.theme().border),
                    )
                    .child(
                        gpui::svg()
                            .path(agent.icon_path())
                            .size(px(size * 0.54))
                            .text_color(gpui::white()),
                    )
                    .when_some(dot, |b, dot| b.child(dot))
                    .tooltip(move |window, cx| {
                        gpui_component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
                    })
                    .into_any_element()
            }
            None => base
                .relative()
                .rounded_full()
                .bg(cx.theme().muted)
                .child(
                    gpui::svg()
                        .path("icons/terminal.svg")
                        .size(px(size * 0.56))
                        .text_color(cx.theme().foreground.opacity(0.65)),
                )
                .when_some(ssh, |b, rgb| {
                    b.child(Self::status_dot(rgb, 0, size, cx.theme().background, false))
                })
                .into_any_element(),
        }
    }

    /// The full title behind a shortened one, for the row to name on hover.
    ///
    /// `tab_label` hands back a path elided to its last three segments and then
    /// capped, and the chip truncates whatever is left over — so a tab could
    /// read `…/a/b/c` with no way to find out which `a` that was. `None` when
    /// nothing was dropped, so tabs that already show their whole name stay
    /// quiet under the pointer.
    pub(crate) fn tab_title_tooltip(
        &self,
        tab: &Tab,
        index: usize,
        window: Option<&Window>,
        cx: &App,
    ) -> Option<SharedString> {
        if tab.name.as_ref().is_some_and(|n| !n.trim().is_empty()) {
            return None;
        }
        let raw = tab.leaf_title(window, cx);
        let raw = raw.trim();
        if raw.is_empty() || raw == self.tab_label(tab, index, window, cx) {
            return None;
        }
        Some(SharedString::from(abbreviate_home(raw).into_owned()))
    }

    pub(crate) fn tab_label(
        &self,
        tab: &Tab,
        index: usize,
        window: Option<&Window>,
        cx: &App,
    ) -> String {
        if let Some(name) = tab.name.as_ref() {
            let trimmed = name.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
        let raw = tab.leaf_title(window, cx);
        let label = short_title(&raw);
        if label.trim().is_empty() {
            t_fmt(
                L10nKey::TabUnnamedShell,
                &[("n", &((index + 1).to_string()))],
            )
        } else {
            label
        }
    }

    pub(crate) fn attach_new_tab_menu(
        &self,
        button: Button,
        cx: &Context<Self>,
    ) -> impl IntoElement + use<> {
        let shells = self.shells.shells.clone();
        let default_name = self.default_shell_label(cx);
        let app = cx.entity().downgrade();
        // Every other tile in this row names itself on hover — Switch
        // Workspace, More, Hide Sidebar. The three New Tab buttons that come
        // through here were the ones left silent.
        let button = button.tooltip(chord_hint(t(L10nKey::AppMenuNewTab), "NewTab", cx));
        button.dropdown_menu(move |menu, _window, _cx| {
            let mut menu = menu.min_w(px(220.));
            for shell in &shells {
                let spec = shell_spec(shell);
                let open = app.clone();
                let item = if shell.label == default_name {
                    let label: SharedString = shell.label.clone().into();
                    PopupMenuItem::element(move |_window, cx| {
                        h_flex()
                            .w_full()
                            .items_center()
                            .justify_between()
                            .gap_3()
                            .child(label.clone())
                            .child(
                                div()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(t(L10nKey::ShellDefault)),
                            )
                    })
                } else {
                    PopupMenuItem::new(shell.label.clone())
                };
                menu = menu.item(item.on_click(move |_, window, cx| {
                    if let Some(app) = open.upgrade() {
                        app.update(cx, |this, cx| {
                            this.new_tab_with_shell(Some(spec.clone()), window, cx);
                        });
                    }
                }));
            }
            if shells.is_empty() {
                let open_default = app.clone();
                menu = menu.item(PopupMenuItem::new(t(L10nKey::AppMenuNewTab)).on_click(
                    move |_, window, cx| {
                        if let Some(app) = open_default.upgrade() {
                            app.update(cx, |this, cx| this.new_tab(window, cx));
                        }
                    },
                ));
            }
            menu
        })
    }

    pub(crate) fn tab_context_menu(
        menu: PopupMenu,
        index: usize,
        below_wording: bool,
        app: &gpui::WeakEntity<Self>,
        window: &Window,
        cx: &App,
    ) -> PopupMenu {
        let Some(entity) = app.upgrade() else {
            return menu;
        };
        let this = entity.read(cx);
        let tab_count = this.tabs.len();
        let cwd = this.tab_cwd(index, window, cx);
        let has_cwd = cwd.is_some();
        let mut menu = menu.min_w(px(200.));

        // Every item here acts on *this* tab, so the work is done by the click
        // handler and the action is carried only so `PopupMenu` can look its
        // chord up and print it. The handler wins when both are set. Without
        // this the tab menu was the one context menu in the app that taught no
        // shortcuts — right-clicking a pane offered "Split Right ⌘D" while
        // right-clicking its tab offered a bare "Split Right".
        menu = menu.item(
            PopupMenuItem::new(t(L10nKey::AppMenuRenameTab))
                .action(Box::new(RenameTab))
                .on_click({
                    let app = app.clone();
                    move |_, window, cx| {
                        let _ = app.update(cx, |this, cx| this.start_rename(index, window, cx));
                    }
                }),
        );

        let tab = this.tabs.get(index);
        if tab.is_some_and(|t| t.agent(cx).is_some()) {
            let done = tab.and_then(|t| t.agent_status(cx))
                == Some(crate::core::cli_agent::AgentStatus::Done);
            menu = menu.item(
                PopupMenuItem::new(t(L10nKey::TabContextMarkUnread))
                    .action(Box::new(MarkTabUnread))
                    .disabled(!done)
                    .on_click({
                        let app = app.clone();
                        move |_, _window, cx| {
                            let _ = app.update(cx, |this, cx| this.mark_tab_unread(index, cx));
                        }
                    }),
            );
        }

        let in_repo = this.tab_is_in_repo(index, window, cx);
        if in_repo {
            menu = menu.separator().item(
                PopupMenuItem::new(t(L10nKey::AppMenuNewWorktreeTab))
                    .action(Box::new(NewWorktreeTab))
                    .on_click({
                        let app = app.clone();
                        move |_, window, cx| {
                            let _ =
                                app.update(cx, |this, cx| this.new_worktree_tab(index, window, cx));
                        }
                    }),
            );
        }

        let agent_session = this.tab_agent_session(index, window, cx);
        if let Some((source, session)) = &agent_session
            && let Some(label) = session.fork_label
        {
            if !in_repo {
                menu = menu.separator();
            }
            let forkable = session.forkable();
            menu = menu.item(
                PopupMenuItem::new(label)
                    .action(Box::new(ForkAgentSession))
                    .disabled(!forkable)
                    .on_click({
                        let app = app.clone();
                        let source = source.clone();
                        move |_, window, cx| {
                            let source = source.clone();
                            let _ = app.update(cx, |this, cx| {
                                this.fork_agent_session(
                                    index,
                                    source,
                                    crate::ui::app::ForkPlacement::NewTab,
                                    window,
                                    cx,
                                )
                            });
                        }
                    }),
            );
        }

        menu = menu
            .separator()
            .item(
                PopupMenuItem::new(t(L10nKey::AppMenuSplitRight))
                    .action(Box::new(SplitRight))
                    .on_click({
                        let app = app.clone();
                        move |_, window, cx| {
                            let _ = app.update(cx, |this, cx| {
                                this.activate(index, window, cx);
                                this.split(Axis::Horizontal, window, cx);
                            });
                        }
                    }),
            )
            .item(
                PopupMenuItem::new(t(L10nKey::AppMenuSplitDown))
                    .action(Box::new(SplitDown))
                    .on_click({
                        let app = app.clone();
                        move |_, window, cx| {
                            let _ = app.update(cx, |this, cx| {
                                this.activate(index, window, cx);
                                this.split(Axis::Vertical, window, cx);
                            });
                        }
                    }),
            );

        menu = menu.separator().item(
            PopupMenuItem::new(t(L10nKey::AppMenuCopyWorkingDirectory))
                .action(Box::new(CopyWorkingDirectory))
                .disabled(!has_cwd)
                .on_click(move |_, _window, cx| {
                    if let Some(cwd) = cwd.as_ref() {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                            cwd.display().to_string(),
                        ));
                    }
                }),
        );

        if let Some(session_id) = agent_session.map(|(_, s)| s.session_id) {
            menu = menu.item(
                PopupMenuItem::new(t(L10nKey::AppMenuCopySessionId))
                    .action(Box::new(CopyAgentSessionId))
                    .disabled(session_id.is_none())
                    .on_click(move |_, _window, cx| {
                        if let Some(id) = session_id.as_ref() {
                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(id.clone()));
                        }
                    }),
            );
        }

        menu.separator()
            .item(
                PopupMenuItem::new(t(L10nKey::TabContextCloseTab))
                    .action(Box::new(CloseActiveTab))
                    .on_click({
                        let app = app.clone();
                        move |_, window, cx| {
                            let _ = app.update(cx, |this, cx| this.close_tab(index, window, cx));
                        }
                    }),
            )
            .item(
                PopupMenuItem::new(t(L10nKey::AppMenuCloseOtherTabs))
                    .action(Box::new(CloseOtherTabs))
                    .disabled(tab_count <= 1)
                    .on_click({
                        let app = app.clone();
                        move |_, window, cx| {
                            let _ =
                                app.update(cx, |this, cx| this.close_other_tabs(index, window, cx));
                        }
                    }),
            )
            .item(
                PopupMenuItem::new(if below_wording {
                    t(L10nKey::TabContextCloseTabsBelow)
                } else {
                    t(L10nKey::AppMenuCloseTabsRight)
                })
                .action(Box::new(CloseTabsToTheRight))
                .disabled(index + 1 >= tab_count)
                .on_click({
                    let app = app.clone();
                    move |_, window, cx| {
                        let _ =
                            app.update(cx, |this, cx| this.close_tabs_right_of(index, window, cx));
                    }
                }),
            )
    }

    pub(crate) fn tab_strip(
        &self,
        show_chips: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let active = self.active;
        let show_badges = self.mod_hint_badges;
        // On macOS an open detail panel draws its own chrome in the title bar,
        // so the strip stops at the panel's edge rather than running the width
        // of the window. Sizing it to the whole viewport made it overrun that
        // edge, and what got pushed out past it was the New Tab button.
        let panel_w = match cfg!(target_os = "macos") && self.right_panel_open(cx) {
            true => self.right_panel_px(window, cx),
            false => 0.,
        };
        let strip_w = if cfg!(target_os = "macos") {
            (window.viewport_size().width - px(80. + panel_w)).max(px(160.))
        } else {
            (window.viewport_size().width - px(114.)).max(px(140.))
        };
        let chrome_band_w = (!cfg!(target_os = "macos") && self.right_panel_open(cx)).then(|| {
            (self.right_panel_px(window, cx) - crate::ui::app::WINDOW_CONTROLS_W - 1.).max(0.)
        });
        // `corner_w` reserves the trailing window chrome. With the panel open on
        // macOS that chrome belongs to the panel's own header, which the strip
        // now stops short of, so reserving for it here would charge the chips
        // for it twice.
        let corner_w = if panel_w > 0. {
            0.
        } else {
            chrome_band_w.unwrap_or_else(|| {
                let trailing_pad = if cfg!(target_os = "macos") {
                    tile_trailing_inset()
                } else {
                    4.
                };
                trailing_pad + crate::ui::app::TILE_SIZE + 2. + crate::ui::app::TILE_SIZE
            })
        };
        let fixed_w = 3. * CHIP_GAP + crate::ui::app::TILE_SIZE + corner_w;
        let chips_avail = (strip_w - px(fixed_w + GRAB_HANDLE_W)).max(px(80.));
        let mut chips = h_flex()
            .items_center()
            .gap(px(CHIP_GAP))
            .min_w_0()
            .max_w(chips_avail)
            .overflow_hidden();

        let slots: Rc<RefCell<Vec<Bounds<Pixels>>>> =
            Rc::new(RefCell::new(vec![Bounds::default(); self.tabs.len()]));
        let preview = reorder::preview(
            &self.reorder,
            &Surface::Strip,
            self.tabs.len(),
            window.mouse_position(),
        );
        let display: Vec<usize> = match &preview {
            Some(p) => {
                reorder::set_pending(&self.reorder, &Surface::Strip, p.order.clone());
                p.order.clone()
            }
            None => (0..self.tabs.len()).collect(),
        };
        let display = visible_chips(&display, active, f32::from(chips_avail));

        for i in display {
            if !show_chips {
                break;
            }
            let dragged = preview.as_ref().is_some_and(|p| p.from == i);
            let tab = &self.tabs[i];
            let is_active = i == active;
            let label = self.tab_label(tab, i, Some(window), cx);
            let full_title = self.tab_title_tooltip(tab, i, Some(window), cx);
            let ssh_dot = self.tab_ssh_dot(tab, cx);
            let agent = tab.agent(cx);
            let agent_status = tab.agent_status(cx);
            let agent_unread = tab.agent_unread_count(cx);

            let rename_input = self
                .renaming
                .as_ref()
                .filter(|r| r.index == i)
                .map(|r| r.input.clone());
            let label_region = match rename_input {
                Some(input) => div()
                    .id(("tab-rename", i))
                    .flex_1()
                    .min_w_0()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(Input::new(&input).appearance(false))
                    .into_any_element(),
                None => div()
                    .id(("tab-label", i))
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .when(is_active, |d| d.font_weight(FontWeight::MEDIUM))
                    .when_some(full_title, |d, title| {
                        d.tooltip(move |window, cx| {
                            gpui_component::tooltip::Tooltip::new(title.clone()).build(window, cx)
                        })
                    })
                    .child(label)
                    .into_any_element(),
            };

            let chip = h_flex()
                .id(("tab-chip", i))
                .on_drag(DragTab, {
                    let state = self.reorder.clone();
                    let slots = slots.clone();
                    move |_drag, grab, _window, cx| {
                        cx.stop_propagation();
                        *state.borrow_mut() = Some(Reorder::new(
                            Surface::Strip,
                            i,
                            slots.borrow().clone(),
                            Axis::Horizontal,
                            px(CHIP_GAP),
                            grab,
                        ));
                        cx.new(|_| DragTab)
                    }
                })
                .occlude()
                .group(SharedString::from(format!("tab-chip-{i}")))
                .cursor_pointer()
                .items_center()
                .justify_between()
                .gap_1p5()
                .h(px(30.))
                .min_w(px(CHIP_MIN_W))
                .flex_shrink(1.)
                .pl_3()
                .pr_1p5()
                .rounded_lg()
                .when(is_active, |s| {
                    s.bg(cx.theme().secondary).text_color(cx.theme().foreground)
                })
                .when(!is_active, |s| {
                    s.text_color(cx.theme().muted_foreground)
                        .hover(|s| s.bg(cx.theme().muted))
                })
                .when(dragged, |s| s.opacity(0.75))
                .child(
                    canvas(
                        {
                            let slots = slots.clone();
                            move |bounds, _window, _cx| {
                                if let Some(slot) = slots.borrow_mut().get_mut(i) {
                                    *slot = bounds;
                                }
                            }
                        },
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .inset_0(),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        if ev.click_count >= 2 {
                            window.titlebar_double_click();
                        } else {
                            this.activate(i, window, cx);
                        }
                    }),
                )
                .when_some(ssh_dot, |c, rgb| {
                    c.child(
                        div()
                            .flex_shrink_0()
                            .size(px(6.))
                            .rounded_full()
                            .bg(gpui::rgb(rgb)),
                    )
                })
                .when_some(agent, |chip, agent| {
                    chip.child(self.tab_avatar(
                        ("tab-avatar", i),
                        Some(agent),
                        agent_status,
                        agent_unread,
                        None,
                        18.,
                        cx,
                    ))
                })
                .child(label_region)
                .when(show_badges && i < 9, |chip| {
                    chip.child(
                        div()
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(20.))
                            .text_xs()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(if is_active {
                                cx.theme().foreground
                            } else {
                                cx.theme().muted_foreground
                            })
                            .child(tab_badge_label(i)),
                    )
                })
                .when(!(show_badges && i < 9), |chip| {
                    let backing = if is_active {
                        cx.theme().secondary
                    } else {
                        cx.theme().muted
                    };
                    let mut fade_from = backing;
                    fade_from.a = 0.;
                    chip.child(
                        h_flex()
                            .absolute()
                            // 3 + MIN_TARGET + 3 centres the button in the 30px chip.
                            .top(px(3.))
                            .right(px(6.))
                            .opacity(0.)
                            .group_hover(SharedString::from(format!("tab-chip-{i}")), |s| {
                                s.opacity(1.)
                            })
                            .child(div().w(px(10.)).h(px(MIN_TARGET)).bg(linear_gradient(
                                90.,
                                linear_color_stop(fade_from, 0.),
                                linear_color_stop(backing, 1.),
                            )))
                            .child(
                                div().bg(backing).child(
                                    hit_target(
                                        Button::new(("tab-close", i))
                                            .icon(IconName::Close)
                                            .ghost()
                                            .xsmall(),
                                    )
                                    .tooltip(t(L10nKey::TabContextCloseTab))
                                    .on_click(cx.listener(
                                        move |this, _, window, cx| {
                                            this.close_tab(i, window, cx);
                                        },
                                    )),
                                ),
                            ),
                    )
                });

            let menu_app = cx.entity().downgrade();
            let chip = chip.context_menu(move |menu, window, cx| {
                Self::tab_context_menu(menu, i, false, &menu_app, window, cx)
            });
            chips = chips.child(match &preview {
                Some(p) if p.from == i => deferred(chip.relative().left(p.held)).into_any_element(),
                Some(p) => {
                    let offset = p.offsets[i].as_f32();
                    chip.with_animation(
                        (
                            SharedString::from(format!("chip-slide-{}", p.generation)),
                            i,
                        ),
                        Animation::new(std::time::Duration::from_millis(REORDER_SLIDE_MS))
                            .with_easing(ease_out_quint()),
                        move |el, delta| el.left(px(offset * (1. - delta))),
                    )
                    .into_any_element()
                }
                None => chip.into_any_element(),
            });
        }

        let add_button = div().occlude().flex_shrink_0().child(
            self.attach_new_tab_menu(
                chrome_tile_sized(
                    Button::new("tab-add").icon(Icon::new(IconName::Plus)),
                    TILE_SIZE,
                    TILE_GLYPH_LINE,
                    false,
                    cx,
                )
                .rounded_lg(),
                cx,
            ),
        );

        let rail_collapsed = !show_chips && !self.left_panel_open(cx);
        let left_group = rail_collapsed.then(|| {
            h_flex()
                .flex_shrink_0()
                .items_center()
                .gap(px(2.))
                .ml(px(crate::ui::app::title_bar_hug_offset()))
                .when_some(crate::ui::app::window_mark(), |group, mark| {
                    group.child(
                        div()
                            .flex_shrink_0()
                            .pl(px(crate::ui::app::CONTENT_INSET
                                - crate::ui::app::tile_trailing_inset()))
                            .pr(px(4.))
                            .child(mark),
                    )
                })
                .child(
                    div().occlude().flex_shrink_0().child(
                        self.attach_new_tab_menu(
                            chrome_tile_sized(
                                Button::new("titlebar-add-collapsed")
                                    .icon(Icon::new(IconName::Plus)),
                                TILE_SIZE,
                                TILE_GLYPH_LINE,
                                false,
                                cx,
                            )
                            .rounded_lg(),
                            cx,
                        ),
                    ),
                )
                .child(
                    div().occlude().flex_shrink_0().child(
                        chrome_tile(
                            Button::new("titlebar-expand-sidebar")
                                .icon(Icon::empty().path("icons/panel-left.svg")),
                            false,
                            cx,
                        )
                        .rounded_lg()
                        .tooltip(chord_hint(
                            t(L10nKey::TabTooltipShowSidebar),
                            "ToggleLeftPanel",
                            cx,
                        ))
                        .on_click(cx.listener(|this, _, _window, cx| this.toggle_left_panel(cx))),
                    ),
                )
        });

        let panel_open = self.right_panel_open(cx);
        let right_chrome =
            (!panel_open || !cfg!(target_os = "macos")).then(|| self.window_chrome(window, cx));

        h_flex()
            .id("tab-strip")
            .items_center()
            .gap_1p5()
            .when(show_chips, |this| this.w(strip_w))
            .when(!show_chips, |this| this.w_full())
            .pl_0()
            .min_w_0()
            .when_some(left_group, |this, g| this.child(g))
            .child(chips)
            .when(show_chips, move |this| this.child(add_button))
            .child(div().flex_1().min_w(px(GRAB_HANDLE_W)))
            .when_some(right_chrome, |this, chrome| match chrome_band_w {
                Some(w) => this.child(
                    h_flex()
                        .flex_none()
                        .w(px(w))
                        .items_center()
                        .pl(px(tile_trailing_inset()))
                        .child(chrome),
                ),
                None => this.child(chrome),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[test]
    fn every_visible_agent_state_has_words_for_it() {
        use crate::core::cli_agent::AgentStatus;
        crate::ui::i18n::set_locale("en");
        // Idle draws no dot, so it has nothing to name.
        assert_eq!(agent_status_label(None), None);
        assert_eq!(agent_status_label(Some(AgentStatus::Idle)), None);
        // Every state that does draw a dot can be read out loud.
        for status in [
            AgentStatus::Working,
            AgentStatus::Waiting,
            AgentStatus::Done,
        ] {
            assert!(status.dot_rgb().is_some());
            assert!(
                agent_status_label(Some(status)).is_some_and(|s| !s.is_empty()),
                "{status:?} paints a dot with no words behind it"
            );
        }
        // Waiting is the state worth acting on; it must not read as Done.
        assert_ne!(
            agent_status_label(Some(AgentStatus::Waiting)),
            agent_status_label(Some(AgentStatus::Done))
        );
    }

    #[test]
    fn a_brand_disc_that_matches_the_window_gets_an_edge() {
        use crate::ui::presets::needs_edge;
        let dark: gpui::Hsla = gpui::rgb(0x111111).into();
        let light: gpui::Hsla = gpui::rgb(0xffffff).into();
        let codex = crate::core::cli_agent::CLIAgent::Codex.accent_rgb();
        let claude = crate::core::cli_agent::CLIAgent::Claude.accent_rgb();

        assert_eq!(codex, 0x000000, "Codex's disc is pure black");
        assert!(
            needs_edge(codex, dark),
            "a black disc on a dark window is not a disc"
        );
        assert!(!needs_edge(codex, light));
        assert!(!needs_edge(claude, dark) && !needs_edge(claude, light));
    }

    #[test]
    fn short_title_strips_user_host_and_shows_shallow_path_in_full() {
        assert_eq!(short_title("user@host:~/projects/app"), "~/projects/app");
        assert_eq!(short_title("/usr/local/bin"), "/usr/local/bin");
        assert_eq!(short_title("plain"), "plain");
    }

    #[test]
    fn short_title_truncates_deep_paths_to_trailing_segments() {
        assert_eq!(short_title("user@host:~/repo/025/tty7"), "…/repo/025/tty7");
        assert_eq!(short_title("/usr/local/share/man"), "…/local/share/man");
        assert_eq!(short_title("a/b/c/d"), "…/b/c/d");
    }

    #[test]
    fn short_title_keeps_home_tilde_and_normalizes_trailing_slash() {
        assert_eq!(short_title("user@host:~"), "~");
        assert_eq!(short_title("~"), "~");
        assert_eq!(short_title("a/b/c/"), "a/b/c");
    }

    #[test]
    fn short_title_blank_input_is_empty_and_long_names_are_clamped() {
        assert_eq!(short_title("   "), "");
        let long = "a".repeat(50);
        let out = short_title(&long);
        assert_eq!(out.chars().count(), 41);
        assert!(out.ends_with('…'));
    }

    /// `TestAppContext` shapes through gpui's `NoopTextSystem`, where every
    /// glyph is exactly one em — weight-agnostic and, more to the point,
    /// CJK-agnostic. That keeps these tests identical on all three CI targets,
    /// but it also means they cannot speak to the proportional and mixed-script
    /// widths the elision exists for: what they pin is the contract — which
    /// parts of a label must survive, and that the result fits its budget.
    fn elide_setup(cx: &mut TestAppContext) -> (gpui::WindowTextSystem, gpui::Font, f32) {
        let size = 14.;
        (
            gpui::WindowTextSystem::new(cx.text_system().clone()),
            gpui::Font::default(),
            size,
        )
    }

    #[gpui::test]
    fn elide_path_fits_shallow_paths_untouched(cx: &mut TestAppContext) {
        let (ts, font, size) = elide_setup(cx);
        let path = "~/tty7";
        let max = measure_text(&ts, &font, size, path) + 1.;
        assert_eq!(elide_path_keep_tail(&ts, &font, size, path, max), "~/tty7");
    }

    #[gpui::test]
    fn elide_path_shows_the_whole_deep_path_when_the_budget_allows(cx: &mut TestAppContext) {
        let (ts, font, size) = elide_setup(cx);
        // A wide sidebar must not elide a deep path: only the width may
        // decide, never a fixed segment cap.
        let path = "E:/work/toolbox/crates/tty7-core/src/client";
        let max = measure_text(&ts, &font, size, path) + 1.;
        assert_eq!(elide_path_keep_tail(&ts, &font, size, path, max), path);
    }

    #[gpui::test]
    fn elide_path_keeps_drive_tail_and_budget(cx: &mut TestAppContext) {
        let (ts, font, size) = elide_setup(cx);
        let path = "E:/work/toolbox/src/ui/tab_sidebar.rs";
        let max = 200.;
        assert!(
            measure_text(&ts, &font, size, path) > max,
            "the fixture has to be wider than the budget to exercise elision"
        );
        let out = elide_path_keep_tail(&ts, &font, size, path, max);
        assert!(out.starts_with("E:/…/"), "drive letter survives: {out}");
        assert!(
            out.ends_with("tab_sidebar.rs"),
            "the file name always survives: {out}"
        );
        assert!(
            measure_text(&ts, &font, size, &out) <= max,
            "the elided label fits the budget"
        );
    }

    #[gpui::test]
    fn elide_path_keeps_tilde_and_leading_slash(cx: &mut TestAppContext) {
        let (ts, font, size) = elide_setup(cx);
        let home = "~/projects/toolbox/src/ui/tab_sidebar.rs";
        assert!(
            measure_text(&ts, &font, size, home) > 200.,
            "the fixture has to be wider than the budget to exercise elision"
        );
        let out = elide_path_keep_tail(&ts, &font, size, home, 200.);
        assert!(out.starts_with("~/…/"), "tilde root survives: {out}");
        assert!(out.ends_with("tab_sidebar.rs"));

        let abs = "/usr/local/share/man/man1/git.1";
        assert!(
            measure_text(&ts, &font, size, abs) > 120.,
            "the fixture has to be wider than the budget to exercise elision"
        );
        let out = elide_path_keep_tail(&ts, &font, size, abs, 120.);
        assert!(out.starts_with("/…/"), "absolute root survives: {out}");
        assert!(out.ends_with("git.1"));
    }

    #[gpui::test]
    fn elide_path_tears_only_the_last_segment_as_a_last_resort(cx: &mut TestAppContext) {
        let (ts, font, size) = elide_setup(cx);
        let path = "E:/supercalifragilisticexpialidocious";
        let max = 60.;
        assert!(measure_text(&ts, &font, size, path) > max);
        let out = elide_path_keep_tail(&ts, &font, size, path, max);
        assert!(out.starts_with('…'), "a torn segment reads as torn: {out}");
        assert!(
            out.chars().nth(1) != Some('/'),
            "no slash after a torn segment: {out}"
        );
        assert!(out.ends_with('s'), "the word's tail survives: {out}");
        assert!(measure_text(&ts, &font, size, &out) <= max);
    }

    #[gpui::test]
    fn elide_edges_keeps_both_ends_of_a_branch(cx: &mut TestAppContext) {
        let (ts, font, size) = elide_setup(cx);
        let branch = "window-transparency-backdrop";
        let max = 140.;
        assert!(measure_text(&ts, &font, size, branch) > max);
        let out = elide_keep_edges(&ts, &font, size, branch, max);
        assert!(out.starts_with("window-"), "head survives: {out}");
        assert!(out.ends_with("backdrop"), "tail survives: {out}");
        assert!(out.contains('…'));
        assert!(measure_text(&ts, &font, size, &out) <= max);
        assert!(out.chars().count() < branch.chars().count());
    }

    #[gpui::test]
    fn elide_edges_leaves_short_branches_alone(cx: &mut TestAppContext) {
        let (ts, font, size) = elide_setup(cx);
        let branch = "main";
        let max = measure_text(&ts, &font, size, branch) + 1.;
        assert_eq!(elide_keep_edges(&ts, &font, size, branch, max), "main");
    }

    #[gpui::test]
    fn elide_edges_falls_back_to_a_tail_sliver_when_the_head_cannot_fit(cx: &mut TestAppContext) {
        let (ts, font, size) = elide_setup(cx);
        let branch = "window-transparency-backdrop";
        let out = elide_keep_edges(&ts, &font, size, branch, 30.);
        assert!(out.starts_with('…'));
        assert!(measure_text(&ts, &font, size, &out) <= 30.);
    }

    #[gpui::test]
    fn elide_path_cuts_windows_backslash_paths_on_segments(cx: &mut TestAppContext) {
        let (ts, font, size) = elide_setup(cx);
        let path = r"C:\Users\dev\AppData\Local\Temp\verify-build";
        let max = 200.;
        assert!(measure_text(&ts, &font, size, path) > max);
        let out = elide_path_keep_tail(&ts, &font, size, path, max);
        assert!(out.starts_with(r"C:\…\"), "drive letter survives: {out}");
        assert!(
            out.ends_with("verify-build"),
            "the leaf segment survives: {out}"
        );
        assert!(measure_text(&ts, &font, size, &out) <= max);
    }

    /// One tab must not spell its location two ways depending on how wide the
    /// sidebar happens to be.
    #[gpui::test]
    fn elide_path_keeps_the_separator_the_path_arrived_with(cx: &mut TestAppContext) {
        let (ts, font, size) = elide_setup(cx);
        let windows = r"C:\Users\dev\projects\toolbox\src\ui\app.rs";
        let wide = measure_text(&ts, &font, size, windows) + 1.;
        assert_eq!(
            elide_path_keep_tail(&ts, &font, size, windows, wide),
            windows,
            "a path that fits is left exactly as it arrived"
        );
        let out = elide_path_keep_tail(&ts, &font, size, windows, 120.);
        assert!(!out.contains('/'), "no forward slash creeps in: {out}");

        let unix = "/home/dev/projects/toolbox/src/ui/app.rs";
        let out = elide_path_keep_tail(&ts, &font, size, unix, 120.);
        assert!(!out.contains('\\'), "no backslash creeps in: {out}");
    }

    /// A branch with no `-`, `_`, `/` or `.` in reach used to lose its head
    /// entirely, which is the one thing this function promises not to do.
    #[gpui::test]
    fn elide_edges_keeps_a_head_on_a_separatorless_token(cx: &mut TestAppContext) {
        let (ts, font, size) = elide_setup(cx);
        let branch = "verylongbranchnamewithoutseps";
        for max in [60., 80., 100., 120.] {
            let out = elide_keep_edges(&ts, &font, size, branch, max);
            assert!(
                out.starts_with('v'),
                "head survives at {max}px: {out}",
                max = max
            );
            assert!(out.ends_with('s'), "tail survives at {max}px: {out}");
            assert!(measure_text(&ts, &font, size, &out) <= max);
        }
    }

    /// A head that fits but leaves nothing behind the ellipsis says less than
    /// a shorter head that keeps the identifying tail.
    #[gpui::test]
    fn elide_edges_gives_up_head_room_to_keep_a_tail(cx: &mut TestAppContext) {
        let (ts, font, size) = elide_setup(cx);
        let branch = "feature/some-really-long-thing";
        let max = 80.;
        assert!(measure_text(&ts, &font, size, branch) > max);
        let out = elide_keep_edges(&ts, &font, size, branch, max);
        assert!(
            !out.ends_with('…'),
            "the tail is never traded away for a longer head: {out}"
        );
        assert!(out.ends_with('g'), "the identifying tail survives: {out}");
        assert!(measure_text(&ts, &font, size, &out) <= max);
    }

    /// The sidebar title is not always a path. `elide_label` has to notice,
    /// because the path rule drops the head — and for a command line or a name
    /// the user typed, the head is the part that names it.
    #[gpui::test]
    fn elide_label_keeps_the_head_of_a_non_path(cx: &mut TestAppContext) {
        let (ts, font, size) = elide_setup(cx);
        for text in ["Backend server logs", "npm run dev -- --watch"] {
            let max = 90.;
            assert!(measure_text(&ts, &font, size, text) > max);
            let out = elide_label(&ts, &font, size, text, max);
            let first = text.chars().next().unwrap();
            assert!(
                out.starts_with(first),
                "a non-path keeps its head: {out} (from {text})"
            );
            assert!(measure_text(&ts, &font, size, &out) <= max);
        }
    }

    /// …while a path still gets the tail-first treatment through the same
    /// entry point.
    #[gpui::test]
    fn elide_label_still_keeps_the_tail_of_a_path(cx: &mut TestAppContext) {
        let (ts, font, size) = elide_setup(cx);
        let path = "~/projects/toolbox/src/ui/tab_sidebar.rs";
        let out = elide_label(&ts, &font, size, path, 200.);
        assert!(out.starts_with("~/…/"), "root survives: {out}");
        assert!(out.ends_with("tab_sidebar.rs"), "leaf survives: {out}");
    }

    #[test]
    fn short_title_cuts_windows_paths_on_backslashes() {
        assert_eq!(
            short_title(r"C:\Users\dev\projects\app"),
            r"…\dev\projects\app"
        );
        assert_eq!(
            short_title(r"C:\Users\dev\repo\deep\path\src\ui"),
            r"…\path\src\ui"
        );
        // A shallow Windows path keeps its drive and its backslashes.
        assert_eq!(short_title(r"C:\Users\app"), r"C:\Users\app");
    }

    /// One chip is 100 wide plus a 6 gap, so this is "room for exactly four".
    const FOUR_CHIPS: f32 = 4. * (CHIP_MIN_W + CHIP_GAP);

    #[test]
    fn every_chip_is_drawn_while_they_all_fit() {
        let order: Vec<usize> = (0..4).collect();
        assert_eq!(visible_chips(&order, 0, FOUR_CHIPS), order);
        assert_eq!(visible_chips(&order, 3, FOUR_CHIPS), order);
        assert_eq!(visible_chips(&[0, 1], 1, FOUR_CHIPS), vec![0, 1]);
    }

    #[test]
    fn the_run_stays_put_until_the_active_chip_would_fall_off() {
        let order: Vec<usize> = (0..9).collect();
        // Anchored at the first tab for as long as the active one is inside it.
        assert_eq!(visible_chips(&order, 0, FOUR_CHIPS), vec![0, 1, 2, 3]);
        assert_eq!(visible_chips(&order, 3, FOUR_CHIPS), vec![0, 1, 2, 3]);
        // Then it slides by exactly as much as it has to.
        assert_eq!(visible_chips(&order, 4, FOUR_CHIPS), vec![1, 2, 3, 4]);
        assert_eq!(visible_chips(&order, 8, FOUR_CHIPS), vec![5, 6, 7, 8]);
    }

    #[test]
    fn the_active_chip_is_always_among_the_drawn_ones() {
        let order: Vec<usize> = (0..40).collect();
        for active in 0..40 {
            for avail in [0., 1., 80., FOUR_CHIPS, 4000.] {
                let shown = visible_chips(&order, active, avail);
                assert!(
                    shown.contains(&active),
                    "active {active} missing at {avail}px: {shown:?}"
                );
            }
        }
    }

    #[test]
    fn a_reordered_run_is_sliced_in_its_own_order() {
        // Mid-drag the strip renders `preview.order`, not 0..n.
        let order = vec![3, 0, 1, 2, 4, 5];
        assert_eq!(visible_chips(&order, 5, FOUR_CHIPS), vec![1, 2, 4, 5]);
    }

    #[test]
    fn configured_shell_arguments_remain_user_authored_in_the_menu() {
        let shell = DetectedShell {
            label: "custom".into(),
            program: "custom-shell".into(),
            args: vec!["--login".into()],
            args_are_tty7_defaults: false,
        };
        let spec = shell_spec(&shell);

        assert_eq!(spec.program, "custom-shell");
        assert_eq!(spec.args, ["--login"]);
        assert!(!spec.args_are_tty7_defaults);
    }
}

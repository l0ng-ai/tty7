//! The tab strip rendered into the title bar: one chip per tab (context icon,
//! label, close affordance), inline rename, drag-to-reorder, and the "+"
//! new-tab button. Split out of `app.rs` as an `impl Tty7App` block (the same
//! pattern `settings` uses) so the window-shell file stays focused on tab/pane
//! orchestration rather than chrome rendering.

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
    OpenSettings, SelectWorkspace1, SelectWorkspace2, SelectWorkspace3, SelectWorkspace4,
    SelectWorkspace5, SelectWorkspace6, SelectWorkspace7, SelectWorkspace8, SelectWorkspace9,
    TogglePalette,
};
use crate::core::config::{Config, RightPanelTab};
use crate::daemon::protocol::ShellSpec;
use crate::ui::app::{TILE_GLYPH, TILE_GLYPH_LINE, TILE_SIZE, Tab, Tty7App, tile_trailing_inset};
use crate::ui::hints::tab_badge_label;
use crate::ui::reorder::{self, Reorder, Surface};

/// How long a slot takes to slide out of the way of a dragged tab, and the
/// gap between chips it has to travel. Short and hard-decelerating: long
/// enough to read as motion, short enough that a fast drag across the strip
/// never queues up a backlog of sliding tabs.
pub(crate) const REORDER_SLIDE_MS: u64 = 140;
const CHIP_GAP: f32 = 6.;

/// How many trailing path components a deep tab label keeps, mirroring
/// ghostty's zsh integration title `%(4~|…/%3~|%~)`: a path deeper than this
/// collapses to `…/` plus its last three components; a shallower one shows in
/// full. The home directory abbreviates to `~`.
const KEEP_SEGMENTS: usize = 3;

/// Abbreviate a leading `$HOME` to `~` (an integrated shell usually already
/// does this, but absolute paths from other shells won't be). Borrows when
/// there's nothing to rewrite.
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

/// Derive a short tab label from a terminal's raw title.
///
/// Shells emit OSC titles like `user@host:~/projects/app`; we show the tail the
/// way ghostty does — the working directory abbreviated with `~`, trimmed to
/// its last few components (`…/repo/025/tty7`) once it runs deep. We strip any
/// `user@host:` prefix first; a non-path title (a running command) passes
/// through unchanged.
fn short_title(raw: &str) -> String {
    let raw = raw.trim();
    if raw.is_empty() {
        return String::new();
    }
    // Drop a leading `user@host:` if present (only when it precedes a path).
    let after_host = match raw.split_once(':') {
        Some((head, tail)) if head.contains('@') => tail,
        _ => raw,
    };
    let after_host = after_host.trim();
    if after_host.is_empty() {
        return String::new();
    }
    let abbreviated = abbreviate_home(after_host);
    let path: &str = abbreviated.as_ref();

    // Classify the leading marker so it can be counted toward depth (like `~` in
    // zsh's `%N~`) but dropped when the path is truncated.
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

    let segments: Vec<&str> = body.split('/').filter(|s| !s.is_empty()).collect();
    if segments.is_empty() {
        return match kind {
            Kind::Home => "~",
            Kind::Absolute => "/",
            Kind::Relative => "",
        }
        .to_string();
    }

    // `~` counts as one component in ghostty's depth test (`%(4~|…|…)`).
    let depth = segments.len() + usize::from(matches!(kind, Kind::Home));
    let mut label = if depth > KEEP_SEGMENTS {
        // Deep path: ellipsis plus the trailing components, leading marker dropped.
        let tail = &segments[segments.len() - KEEP_SEGMENTS..];
        format!("…/{}", tail.join("/"))
    } else {
        match kind {
            Kind::Home => format!("~/{}", segments.join("/")),
            Kind::Absolute => format!("/{}", segments.join("/")),
            Kind::Relative => segments.join("/"),
        }
    };
    // Final safety clamp for an unusually long single component.
    if label.chars().count() > 40 {
        label = format!("{}…", label.chars().take(40).collect::<String>());
    }
    label
}

/// Marks a live drag as a *tab* drag: its type is what the rail's and strip's
/// drop handlers match on, and its presence is what keeps gpui redrawing while
/// the pointer moves. It deliberately carries no state and renders nothing —
/// the tab being dragged never leaves the list, so there is no card floating
/// over the window; the reorder is drawn entirely by the list itself (see
/// [`crate::ui::reorder`]). `pub(crate)` so the vertical
/// [`tab_sidebar`](crate::ui::tab_sidebar) shares the same payload.
#[derive(Clone)]
pub(crate) struct DragTab;

impl Render for DragTab {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        // gpui always paints *something* at the cursor for an active drag;
        // an empty, zero-sized element is how this drag paints nothing.
        div()
    }
}

/// The shared styling for every icon tile in the window's chrome — title bar,
/// rail controls, detail-panel tabs, the editor's close button.
///
/// `ghost()` can't be used: its hover is `secondary_hover` and its selected state
/// `secondary_active`, both solid mid-greys that read far heavier than anything
/// else here. So this spells out all four states in the tab rail's language —
/// nothing at rest, a soft grey capsule on hover, a step darker when selected —
/// which is the same "inset soft-grey capsule" the sidebar rows and the popups
/// use. (Overriding just the hover from outside doesn't work: `Button` applies
/// its own `.hover()` during render, after any the caller set.)
pub(crate) fn chrome_tile_variant(cx: &gpui::App) -> ButtonCustomVariant {
    chrome_tile_variant_for(false, cx)
}

/// The tile's paint, with the glyph weight that matches its state.
///
/// At rest the glyphs sit at `sidebar_foreground` — the same step the tab
/// rail's inactive rows use. Full `foreground` (#111 on white) was a hard,
/// near-black row of ink against the near-white bar around it: the darkest
/// treatment in the window sitting on its lightest surface. A lit toggle still
/// gets full strength, so "on" reads as both a filled capsule *and* darker ink.
pub(crate) fn chrome_tile_variant_for(selected: bool, cx: &gpui::App) -> ButtonCustomVariant {
    ButtonCustomVariant::new(cx)
        .color(cx.theme().transparent)
        .foreground(if selected {
            cx.theme().foreground
        } else {
            cx.theme().sidebar_foreground
        })
        // The sidebar's own selected-row fill (`Surface::selected`, ≈#DBDBDB on
        // white) at full strength: every icon tile in the chrome answers the
        // pointer with the exact grey the rows do. It used to be that grey at
        // 55% opacity, which on a light background is a ≈#EE tint nobody can
        // see — and until the fork learned to read `hover` at all, nothing was
        // painted anyway.
        //
        // Note this is a *hover* wearing the selected rung, which is only
        // legible because that rung is the quiet one; when the ladder briefly
        // folded resting selection into the palette cursor's 1.70:1, hovering a
        // title-bar tile flashed a `#C0C0C0` slab.
        .hover(cx.theme().sidebar_accent)
        // Selected and pressed paint the *same* grey, not a darker step. The
        // chrome has one fill and one only: with two, a lit toggle and a hovered
        // menu button sat side by side in the same corner wearing different
        // greys, which reads as two styles rather than two states. What says a
        // tile is on is that it is filled at all — the tiles around it are bare.
        .active(cx.theme().sidebar_accent)
}

/// What `Button::render` multiplies its own size by before handing it to the
/// icon. The number matters here because the icon size a caller sets *doesn't*:
/// `render` ends with `.with_size(icon_size)` on whatever `Icon` it was given,
/// overwriting it unconditionally. So `Icon::size(px(18.))` on a `.xsmall()`
/// button silently rendered at `Size::XSmall` — 12px — and every chrome glyph in
/// the window had been that size regardless of what its call site asked for.
/// Sizing the *button* is the only channel that reaches the glyph.
pub(crate) const BUTTON_ICON_SCALE: f32 = 0.75;

/// A chrome tile at the standard size: [`TILE_SIZE`] box, [`TILE_GLYPH`] glyph.
pub(crate) fn chrome_tile(button: Button, selected: bool, cx: &gpui::App) -> Button {
    chrome_tile_sized(button, TILE_SIZE, TILE_GLYPH, selected, cx)
}

/// The same tile with its geometry named — for the line-art glyphs, which need a
/// larger nominal size to draw the same ink, and for the body-scale tiles inside
/// a panel. Callers set their own rounding; everything else is decided here so
/// no call site can drift from the rhythm again.
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

/// The colour of a live workspace's corner dot. The same green
/// [`AgentStatus::Done`](crate::core::cli_agent::AgentStatus::dot_rgb) uses —
/// deliberately *not* the brand mint, which belongs to the logo and would be
/// the only saturated pixel in a chrome that has none.
pub(crate) const LIVE_DOT: u32 = 0x22C55E;

/// The colour of the dot on a workspace whose machine could not be asked.
///
/// A neutral grey from the same family as [`LIVE_DOT`], because "unknown" is a
/// status and the app has exactly one shape for status — the corner dot. Green
/// says running, absence says stopped (§ [`workspace_avatar`]), and this says
/// neither: the machine is out of reach and the sessions on it are very
/// probably fine. Deliberately *not* amber or red — nothing has gone wrong with
/// the user's work, only with our view of it.
pub(crate) const UNKNOWN_DOT: u32 = 0x9AA0A6;

/// The monogram badge for a workspace, built in the same shape as a tab
/// avatar: a neutral disc carrying the first letter, with liveness riding the
/// corner as a small dot.
///
/// The dot is the sidebar's [`status_dot`](Tty7App::status_dot), ringed in the
/// surface it sits on — one corner-dot language for every avatar in the app.
/// Drawn bare it was the same disc, but a bare disc at this diameter is all
/// colour and no edge, and it landed on the menu as the loudest thing in it;
/// the ring spends half the dot's width on separation instead.
///
/// A stopped workspace draws *no* dot, matching `AgentStatus::Idle`: a resting
/// thing is just its mark. An "off" indicator would be a second shape invented
/// for one list, and this app already decided that absence says it.
///
/// A workspace whose machine could not be asked draws the same dot in
/// [`UNKNOWN_DOT`] grey. It has to be visibly *not* the stopped state: pane ids
/// are per-machine, so a box that is off or unreachable can tell us nothing
/// about its sessions, and drawing that as a bare badge would say "your work is
/// gone" every time a link blinks. One shape, three readings — green, grey,
/// nothing.
///
/// "This is the one you are looking at" is drawn by *subtraction*: the current
/// badge renders at full strength and every other one fades to the same 0.55
/// an unfocused pane uses. A leading checkmark would be truer to menu
/// convention, but it makes the popup reserve a whole gutter that seven of
/// eight rows leave empty and every label indent past; and marking the current
/// row by *adding* something — an inverted disc, a ring — puts the heaviest
/// pixels in the menu on the one row that needs no introduction.
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
                // `secondary`, not `muted`: a menu's fill is `popover`, and those
                // two differ by half a percent — the disc came out invisible,
                // leaving a bare letter with a dot floating beside it.
                .bg(cx.theme().secondary)
                .flex()
                .items_center()
                .justify_center()
                .text_size(px((size * 0.46).round()))
                .font_weight(FontWeight::MEDIUM)
                .text_color(cx.theme().foreground.opacity(0.65))
                .child(initial)
                // The same 0.55 an unfocused pane fades to, and for the same
                // reason given there: a background-tinted scrim would be
                // white-on-white in a light theme.
                //
                // The disc fades, the dot does not — element opacity multiplies
                // through the whole subtree, so a faded dot takes its separator
                // ring down with it and the green underneath prints straight
                // through it as a halo. Liveness is status either way; it has no
                // reason to say which row you're standing on.
                .when(!current, |disc| disc.opacity(0.55)),
        )
        .children(dot.map(|rgb| {
            // Ringed in `popover`, not `background`: a menu row's fill is the
            // popover colour, and the two differ enough that a background-ringed
            // dot draws a pale halo instead of an edge.
            Tty7App::status_dot(rgb, 0, size, cx.theme().popover)
        }))
}

/// The `SelectWorkspace{1..9}` action for a Window-menu slot, or `None` past
/// the ninth. Shared by the title-bar chip and `ui::theme`'s Window menu so
/// both index `ui::windows::menu_order` identically.
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
    /// Diameter of the workspace avatar. Sized to the rail's row glyphs rather
    /// than to a chrome tile: this control is the head of the tab list, so it
    /// reads down the column instead of across a row of buttons.
    pub(crate) const AVATAR_PX: f32 = 20.0;

    /// The rail's head: which workspace this window is on, plus the menu that
    /// owns everything workspace-scoped — and the app-level entries the "⋯"
    /// used to hold.
    ///
    /// This exists because the rest of it was too well hidden. Switching lived
    /// in the command palette, reopening lived on the home page, ending lived
    /// behind a hover, and renaming had no UI at all — each individually
    /// defensible, together undiscoverable. Nothing in the window even *said*
    /// which workspace it was, which starts to matter the moment there are two.
    ///
    /// It used to be a monogram in the window's top-right corner. Two things
    /// were wrong with that. Physically: the corner it sat in is the *panel's*
    /// top zone while the panel is open, so a control that has nothing to do
    /// with the panel was eating width the panel's own tabs needed — at the
    /// 200px minimum the row wanted 268px. Semantically: a window's workspace is
    /// exactly what the rail below enumerates (this workspace's tabs), so the
    /// name belonged at the head of that column, not across the window from it.
    ///
    /// Here it can afford the full name — the rail is a column with a width of
    /// its own, and it truncates rather than pushing anything off an edge. The
    /// monogram stays as a leading avatar because it is the mark that identifies
    /// *this* workspace at a glance across windows.
    ///
    /// While a rename is in flight the control becomes the text field, so the
    /// name is edited where it is displayed.
    pub(crate) fn workspace_head(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        if let Some(rename) = self.workspace_rename.as_ref() {
            // The tile itself becomes the field — same height, same radius, and
            // the hover fill standing in for "this control is being edited".
            // A bordered input would drop a form control into a strip that has
            // none, and `Input`'s default pill fights every other shape here;
            // `appearance(false)` is what the tab rename uses for the same
            // reason.
            return h_flex()
                .id("workspace-rename")
                .flex_shrink_0()
                .items_center()
                // Full rail width, not the old fixed 150px: this control is a row
                // in a column now, so it takes the column's width like every other
                // row does.
                .h(px(30.))
                .w_full()
                .px(px(7.))
                .rounded_md()
                .bg(cx.theme().sidebar_accent)
                // Swallow mouse-downs (including the double-click that selects a
                // word) so they never reach the enclosing TitleBar and zoom the
                // window — the tab rename learned this the hard way.
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(Input::new(&rename.input).appearance(false).xsmall())
                .into_any_element();
        }

        // Keep the liveness cache warm from here rather than only from the menu
        // builder. The menu is a popup: it takes its snapshot when it opens and
        // never rebuilds, so an answer that starts arriving *at* open lands in a
        // menu nobody is looking at any more and the first open reads `Unknown`
        // for every remote row. Sweeping from the chip means the answers are
        // already in by the time it is clicked. Cheap by construction — the
        // sweep is rate-limited, and past that gate each machine is only asked
        // once its own TTL has run out.
        crate::terminal::pane_liveness::sweep(cx);
        let current = crate::core::session::WorkspaceStore::all(cx)
            .get(self.workspace)
            .map(|w| w.display_name())
            .unwrap_or_else(|| "tty7".to_string());
        // First character, uppercased — the whole point is a glyph that is
        // recognisably *this* workspace at a glance across windows.
        let monogram: String = current
            .chars()
            .next()
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or_else(|| "~".to_string());

        div()
            .occlude()
            .w_full()
            // Right-click does nothing here — and *looked* like it did. A
            // `Button` focuses on any mouse-down and gpui-component draws a
            // 1.5px ring on a focused one, so a right-click left the chip
            // wearing a bright ring for a menu that never opened (a left click
            // hides the same ring by handing focus to the switcher's search
            // field, which is why only right-click showed it).
            //
            // Swallowed in the *capture* phase so the button never sees the
            // press at all: a bubble-phase listener runs after the button has
            // already taken focus.
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
                            // The name shrinks and truncates rather than pushing
                            // the chevron out — same rule the group headers below
                            // it follow, so a long workspace name and a long repo
                            // name behave identically.
                            .child(
                                div()
                                    .flex_shrink(1.)
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(12.5))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(SharedString::from(current.clone())),
                            )
                            // A chevron, unlike the tiles in the row above: those
                            // do one thing on click, this opens something, and the
                            // glyph is what says so.
                            .child(
                                Icon::new(IconName::ChevronDown)
                                    .size(px(11.))
                                    .flex_shrink_0()
                                    .text_color(cx.theme().muted_foreground),
                            ),
                    )
                    .xsmall()
                    .w_full()
                    .h(px(30.))
                    .rounded_md()
                    .tooltip(SharedString::from(current))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.toggle_switcher(window, cx);
                    })),
            )
            .into_any_element()
    }

    /// The corner's app-level "⋯": Command Palette and Settings.
    ///
    /// These two lived in the chip's dropdown, folded in there so the corner had
    /// one menu instead of two adjacent ones. That trade held while the chip
    /// opened a list of workspace names; it stops holding now that the chip
    /// opens a whole panel with its own search and its own per-row actions.
    /// Hanging "Settings…" off the bottom of *that* stitches two different
    /// altitudes together — one answers "which workspace am I going to", the
    /// other "how is this app configured".
    ///
    /// Deliberately just these two. Help/About live in the menu bar, and
    /// duplicating them here only makes the menu longer without making anything
    /// reachable — but on Windows and Linux there is no menu bar to fall back
    /// to, which is why the pair needs a home inside the window at all.
    pub(crate) fn app_menu_tile(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        // `.menu(label, Action)` dispatches the real action, so a click and its
        // shortcut travel one path and the row renders the hint for free — but
        // only if the menu knows which focus handle to dispatch into.
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
            .tooltip("More")
            .dropdown_menu_with_anchor(
                gpui::Anchor::TopRight,
                move |menu, _window, _cx| {
                    menu.min_w(px(200.))
                        .action_context(action_ctx.clone())
                        .menu("Command Palette…", Box::new(TogglePalette))
                        .menu("Settings…", Box::new(OpenSettings))
                },
            ),
        )
    }

    /// The window's right-corner chrome: the detail-panel toggle and the overflow
    /// "⋯". Built here rather than inline because it has two hosts — the title
    /// strip while the panel is closed, and the panel's own top zone while it's
    /// open, since whichever of the two reaches the window's right edge should
    /// carry it. (Same arrangement as the rail: its controls sit on the rail when
    /// it's out, and move into the strip when it's collapsed.)
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
            // Two controls, both window-scoped, and that is the whole corner now.
            // The workspace chip used to lead this group; it moved to the rail's
            // head (`tab_sidebar`), where the list it names actually lives. What
            // forced the move is that this group's other host is the *panel's* top
            // zone, and a panel the user can drag down to 200px cannot seat the
            // panel's four tabs plus three window controls — the row wanted 268px.
            // Nothing that has no business being scoped to the panel gets to
            // compete for that width.
            //
            // The "⋯" glyph ends on the window's content inset like every other
            // right edge in the chrome — hence `inset - TILE_PAD`, which puts the
            // *glyph's ink* there instead of its hit box.
            .pr(px(tile_trailing_inset()))
            // On Windows/Linux the window controls (─ ▢ ✕) sit on the right, right
            // where the "⋯" lands, so its inset has to match *their* rhythm rather
            // than add breathing room: the 34px control tiles put consecutive glyph
            // centres 34px apart, and the "⋯" centre sits `16 + pr + 17` from the
            // minimise glyph. `pr_1` (4px) lands it at ~37px — reading as part of
            // the same row, with just enough slack to not be mistaken for a fourth
            // window control. `pr_3` (12px) put it at ~45px, visibly adrift from the
            // group (this is where the de3896c chrome redesign reset it — see 9b1c7bf).
            .when(!cfg!(target_os = "macos"), |this| this.pr_1())
            .child(
                div().occlude().flex_shrink_0().child(
                    // Never drawn selected, exactly like the rail's own toggle
                    // (`tab_sidebar`): the panel being open is already on screen —
                    // it *is* the panel — so a lit capsule only restates it, and
                    // the two panel toggles would disagree about what a chrome tile
                    // means. The state lives in the tooltip's verb instead.
                    chrome_tile(
                        Button::new("titlebar-right-panel")
                            .icon(Icon::empty().path("icons/panel-right.svg")),
                        false,
                        cx,
                    )
                    .rounded_lg()
                    .tooltip(if panel_open {
                        "Hide Detail Panel"
                    } else {
                        "Show Detail Panel"
                    })
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.toggle_right_panel(cx);
                    })),
                ),
            )
            .child(self.app_menu_tile(window, cx))
    }

    /// The detail panel's tab tiles — icon-only, one per view. Lives here beside
    /// the rest of the chrome tiles so all of them share one styling helper.
    ///
    /// **Chrome scale**, like everything else in that row. These were briefly
    /// dropped to body scale ([`TILE_SIZE_SM`]) to buy width back after the row
    /// overflowed a 200px panel — the tiles live inside a panel, and one size
    /// step separates them from the window chrome beside them without spending a
    /// divider on it. Both arguments hold; neither survives what it looks like.
    /// A 24px tile carrying an 11px glyph next to a 32px one carrying a 13px
    /// glyph doesn't read as a layer below, it reads as shrunk — the panel's
    /// primary navigation, drawn smaller than the two buttons in the corner.
    ///
    /// The width the shrink was buying is bought by [`MIN_WIDTH`] instead: the
    /// row needs 214px at this scale, so the panel's floor is what has to move.
    /// That is the honest place for the constraint anyway — the tiles are as big
    /// as they are, and the panel is as narrow as it can afford to be.
    ///
    /// [`TILE_SIZE_SM`]: crate::ui::app::TILE_SIZE_SM
    /// [`MIN_WIDTH`]: crate::ui::right_panel::MIN_WIDTH
    pub(crate) fn right_panel_tabs(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let active_tab = self.right_panel_tab;
        // Changed-file count, carried in the Changes tooltip. It used to be a
        // tally in that tab's header row, and that row is gone on macOS
        // (`panel_title`) — but this is the one count worth keeping reachable
        // *without* switching to the tab, since it answers "did I touch anything"
        // from wherever you are. Outline's count didn't survive the move: it
        // needs the active leaf, which needs a `&Window` this function doesn't
        // take, and its list is right there the moment you switch.
        let changed = match &self.right_panel.diff {
            Some(Some(snap)) => {
                let n = snap.files.len() + snap.untracked.len();
                (n > 0).then_some(n)
            }
            _ => None,
        };
        [
            (
                RightPanelTab::Info,
                Icon::empty().path("icons/info.svg"),
                "Info",
            ),
            (
                RightPanelTab::Outline,
                Icon::empty().path("icons/list.svg"),
                "Outline",
            ),
            // git-branch (from tty7's own assets) instead of the abstract
            // `Replace` glyph: Changes is a working-tree diff, and the branch
            // mark reads as version control at a glance — matching the mockup.
            (
                RightPanelTab::Changes,
                Icon::empty().path("icons/git-branch.svg"),
                "Changes",
            ),
            (
                RightPanelTab::Files,
                Icon::new(IconName::FolderClosed),
                "Files",
            ),
        ]
        .into_iter()
        .map(|(tab, icon, label)| {
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
                            SharedString::from(format!("{label} · {n}"))
                        }
                        _ => SharedString::from(label),
                    })
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        this.set_right_panel_tab(tab, cx);
                    })),
                )
                .into_any_element()
        })
        .collect()
    }

    /// The status dot pinned to a tab avatar's bottom-right corner (an agent's
    /// live status, or an SSH pane's connection phase): a solid
    /// `rgb` disc with a surface-colored separator ring so it reads as sitting
    /// on the badge. `unread` is how many panes hold a finished turn you
    /// haven't looked at: when nonzero, the dot swells into a count badge —
    /// the same disc grown just enough to speak its number — so read↔unread
    /// stays one element opening its mouth, not a second indicator appearing.
    /// `size` is the avatar edge, `ring` the colour of the surface the badge
    /// sits on — the separator is drawn in it, so a dot on a popover row rings
    /// in the popover's fill rather than the window background's.
    fn status_dot(rgb: u32, unread: usize, size: f32, ring: gpui::Hsla) -> gpui::AnyElement {
        let d = (size * 0.42).max(7.);
        let bg = ring;
        if unread > 0 {
            // The count badge: sized to seat a digit legibly, centred on the
            // read dot's centre (same corner point) so the swell reads as the
            // dot growing in place rather than a new element popping up.
            let nd = (size * 0.72).max(13.0);
            // Panes per tab are single-digit in practice; clamp so an absurd
            // split can never overflow the disc.
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
                .into_any_element()
        }
    }

    /// The leading avatar for a tab row/chip: a rounded badge that brands the
    /// tab by what's running in it — each session fronted with an icon. A
    /// recognized coding agent gets its brand mark — a white silhouette
    /// (gpui tints SVGs as an alpha mask) on the vendor accent; a plain shell
    /// gets a neutral terminal glyph. Live status rides the corner as a
    /// [`status_dot`](Self::status_dot) — the agent's working/waiting/done, or
    /// an SSH pane's connection phase (`ssh`) — one corner-dot language for
    /// the whole avatar column. `size` is the badge's edge in px.
    pub(crate) fn tab_avatar(
        &self,
        agent: Option<crate::core::cli_agent::CLIAgent>,
        status: Option<crate::core::cli_agent::AgentStatus>,
        unread: usize,
        ssh: Option<u32>,
        size: f32,
        cx: &App,
    ) -> gpui::AnyElement {
        let base = div()
            .flex_shrink_0()
            .size(px(size))
            .flex()
            .items_center()
            .justify_center();
        // A circle for every kind — the brand mark / glyph sits
        // small and centred with generous padding rather than filling the badge.
        match agent {
            Some(agent) => {
                // The agent's live status as a small dot pinned to the badge's
                // bottom-right corner (blue working / amber waiting / green
                // done), ringed in the surface color so it reads as sitting on
                // the badge rather than clipped by it. Idle (or unknown) draws
                // no dot — a resting agent is just its brand mark. *Unread*
                // finished turns swell the dot into a count badge ("2 results
                // waiting") that shrinks back to a plain dot once you view the
                // panes, without ever hiding the done state.
                let dot = status
                    .and_then(|s| s.dot_rgb())
                    .map(|rgb| Self::status_dot(rgb, unread, size, cx.theme().background));
                base.relative()
                    .rounded_full()
                    .bg(gpui::rgb(agent.accent_rgb()))
                    .child(
                        gpui::svg()
                            .path(agent.icon_path())
                            .size(px(size * 0.54))
                            .text_color(gpui::white()),
                    )
                    .when_some(dot, |b, dot| b.child(dot))
                    .into_any_element()
            }
            None => base
                .relative()
                .rounded_full()
                // A clearly-visible neutral disc (a neutral grey shell badge), not a
                // near-transparent tint — so the avatar column reads as a column.
                .bg(cx.theme().muted)
                .child(
                    // A flush `>_` prompt (not the boxed `square-terminal`) so it
                    // fills the badge at the same visual weight as a brand mark.
                    gpui::svg()
                        .path("icons/terminal.svg")
                        .size(px(size * 0.56))
                        .text_color(cx.theme().foreground.opacity(0.65)),
                )
                // SSH connection phase as a corner status dot — the same
                // element as an agent's, not a border ring around the badge
                // (a ring read as a second, differently-shaped avatar style).
                .when_some(ssh, |b, rgb| {
                    b.child(Self::status_dot(rgb, 0, size, cx.theme().background))
                })
                .into_any_element(),
        }
    }

    /// The display label for a tab: the user-set name if present, otherwise the
    /// focused terminal's title (shortened), falling back to
    /// "Session N" when there's no title yet. Pass `window` so the label tracks
    /// the focused pane in a split; `None` (no window available) uses the first
    /// leaf.
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
            format!("Session {}", index + 1)
        } else {
            label
        }
    }

    /// Attach the "new tab" shell picker to a button: the configured default
    /// shell leads the menu (tagged `default`), followed by every shell detected
    /// on this machine; clicking one opens a tab on that shell. Extracted so the
    /// title-bar strip's "+" and the vertical [`tab_sidebar`] share one menu
    /// definition rather than duplicating the shell iteration.
    ///
    /// [`tab_sidebar`]: crate::ui::tab_sidebar
    pub(crate) fn attach_new_tab_menu(
        &self,
        button: Button,
        cx: &Context<Self>,
    ) -> impl IntoElement + use<> {
        let shells = self.detected_shells.clone();
        let default_name = crate::core::shells::default_shell_name(
            cx.global::<Config>()
                .shell
                .as_ref()
                .map(|s| s.program.as_str()),
        );
        let app = cx.entity().downgrade();
        button.dropdown_menu(move |menu, _window, _cx| {
            let mut menu = menu.min_w(px(220.));
            // One row per detected shell. There's no separate "New Tab (…)"
            // entry — it only duplicated the default shell's row, and ⌘T already
            // opens a default tab in one press. The configured default is tagged
            // instead so the menu still says which shell a bare new tab would use.
            for shell in &shells {
                let spec = ShellSpec {
                    program: shell.program.clone(),
                    args: shell.args.clone(),
                    // Every arg on a dropdown row was written by
                    // `core::shells::detect_shells`, not the user.
                    args_are_tty7_defaults: true,
                };
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
                                    .child("default"),
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
            // Before shell detection lands (or if it finds nothing), keep a
            // single default entry so the menu is never empty.
            if shells.is_empty() {
                let open_default = app.clone();
                menu = menu.item(
                    PopupMenuItem::new("New Tab").on_click(move |_, window, cx| {
                        if let Some(app) = open_default.upgrade() {
                            app.update(cx, |this, cx| this.new_tab(window, cx));
                        }
                    }),
                );
            }
            menu
        })
    }

    /// Build the per-tab right-click menu, shared by the strip's chips and the
    /// sidebar's rows (which passes `below_wording` so the trailing close reads
    /// "Close Tabs Below" in the vertical list). Live state — tab count, the
    /// tab's cwd — is read at open time through the weak `app` handle, so the
    /// render loop never pays a per-frame cwd syscall and the enablement can't
    /// go stale between render and click.
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

        // Rename — the inline label edit's only entry point (a label
        // double-click zooms the window instead, like the rest of the
        // titlebar).
        menu = menu.item(PopupMenuItem::new("Rename Tab").on_click({
            let app = app.clone();
            move |_, window, cx| {
                let _ = app.update(cx, |this, cx| this.start_rename(index, window, cx));
            }
        }));

        // Mark as Unread — re-arm the avatar's green Done badge so a result
        // you want to revisit nags again. Only agent tabs get the entry, and
        // only a settled (`Done`) tab has a finished turn to mark; a busier
        // status (working/waiting) owns the dot anyway, so the entry disables
        // rather than promising a badge that can't show yet.
        let tab = this.tabs.get(index);
        if tab.is_some_and(|t| t.agent(cx).is_some()) {
            let done = tab.and_then(|t| t.agent_status(cx))
                == Some(crate::core::cli_agent::AgentStatus::Done);
            menu = menu.item(
                PopupMenuItem::new("Mark as Unread")
                    .disabled(!done)
                    .on_click({
                        let app = app.clone();
                        move |_, _window, cx| {
                            let _ = app.update(cx, |this, cx| this.mark_tab_unread(index, cx));
                        }
                    }),
            );
        }

        // Worktree: an isolated checkout of this tab's repo on a fresh branch,
        // opened as a new tab — parallel-agent fuel. Only offered when the
        // tab's cwd actually sits in a git repository; outside one the entry
        // would be pure noise. Read from the git-status cache rather than
        // probed here: a menu is built synchronously, and asking the pane's
        // host is a blocking call (a round trip, if that host is remote).
        let in_repo = this.tab_is_in_repo(index, window, cx);
        if in_repo {
            menu = menu
                .separator()
                .item(PopupMenuItem::new("New Worktree Tab").on_click({
                    let app = app.clone();
                    move |_, window, cx| {
                        let _ = app.update(cx, |this, cx| this.new_worktree_tab(index, window, cx));
                    }
                }));
        }

        // Fork Session — the same kind of operation as New Worktree Tab (spin a
        // parallel line of work off this one), so it sits in the same block. A
        // *tab*-level ask carries no placement question, so it lands in a new
        // tab; the pane right-click menu is where the split directions live
        // (issue #211). Offered only for agents tty7 has a verified fork command
        // for; disabled — not hidden — while the session id is still unknown, so
        // the capability stays discoverable when the hooks aren't installed.
        // The pane comes back with the state and is captured by the row's
        // closure, like `cwd` and `session_id` below: clicking the row focuses
        // the popup, which lives outside every terminal's focus path, so
        // resolving the source pane again at click time would fork the tab's
        // *first* leaf rather than the one this row was labelled for.
        let agent_session = this.tab_agent_session(index, window, cx);
        if let Some((source, session)) = &agent_session
            && let Some(label) = session.fork_label
        {
            // Open the block ourselves when the worktree row above didn't
            // (this tab's cwd isn't in a repo), so the row never glues onto
            // "Mark as Unread".
            if !in_repo {
                menu = menu.separator();
            }
            let forkable = session.forkable();
            menu = menu.item(PopupMenuItem::new(label).disabled(!forkable).on_click({
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
            }));
        }

        // Splits act on the right-clicked tab: activate it first (a no-op when
        // it already is), then split its focused pane — one code path with the
        // keyboard actions.
        menu = menu
            .separator()
            .item(PopupMenuItem::new("Split Right").on_click({
                let app = app.clone();
                move |_, window, cx| {
                    let _ = app.update(cx, |this, cx| {
                        this.activate(index, window, cx);
                        this.split(Axis::Horizontal, window, cx);
                    });
                }
            }))
            .item(PopupMenuItem::new("Split Down").on_click({
                let app = app.clone();
                move |_, window, cx| {
                    let _ = app.update(cx, |this, cx| {
                        this.activate(index, window, cx);
                        this.split(Axis::Vertical, window, cx);
                    });
                }
            }));

        menu = menu.separator().item(
            PopupMenuItem::new("Copy Working Directory")
                .disabled(!has_cwd)
                .on_click(move |_, _window, cx| {
                    if let Some(cwd) = cwd.as_ref() {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                            cwd.display().to_string(),
                        ));
                    }
                }),
        );

        // Copy Session ID, beside Copy Working Directory — the agent's own
        // native id, the one its `--resume` takes. Offered for every agent tab
        // (there is nothing agent-specific about an id) and disabled until one
        // has been reported. The id is read here at open time, so the row can't
        // copy a stale one.
        if let Some(session_id) = agent_session.map(|(_, s)| s.session_id) {
            menu = menu.item(
                PopupMenuItem::new("Copy Session ID")
                    .disabled(session_id.is_none())
                    .on_click(move |_, _window, cx| {
                        if let Some(id) = session_id.as_ref() {
                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(id.clone()));
                        }
                    }),
            );
        }

        menu.separator()
            .item(PopupMenuItem::new("Close Tab").on_click({
                let app = app.clone();
                move |_, window, cx| {
                    let _ = app.update(cx, |this, cx| this.close_tab(index, window, cx));
                }
            }))
            .item(
                PopupMenuItem::new("Close Other Tabs")
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
                    "Close Tabs Below"
                } else {
                    "Close Tabs to the Right"
                })
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

    /// The horizontal tab strip rendered into the title bar. `show_chips` draws
    /// the per-tab chip row; passing `false` (the vertical-sidebar mode, where
    /// the sidebar owns the tab list) keeps only the "+" and "⋯" chrome so the
    /// title bar isn't left empty.
    pub(crate) fn tab_strip(
        &self,
        show_chips: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let active = self.active;
        // While the bare ⌘/Ctrl hold is armed (see `ui::hints`), each of the
        // first nine chips swaps its close affordance for a ⌘N badge — same
        // slot, so nothing shifts when the hints appear.
        let show_badges = self.mod_hint_badges;
        // Explicit viewport-derived strip width, NOT `w_full`: the title bar sizes
        // its content by intrinsic width, so `w_full` doesn't track the window and
        // the strip's right edge (where the "⋯" is pinned) lags behind the
        // shrinking window — the button drifts right into the corner. Deriving the
        // width from the live viewport makes the right edge track the window at
        // every size. macOS reserves 80px on the *left* for the traffic lights, so
        // `viewport - 80` reaches the true right edge and the strip's own `pr`
        // sets the "⋯" inset; other platforms put the window controls on the
        // *right*, so keep the strip narrower to clear them.
        //
        // The non-macOS reserve must cover everything the TitleBar lays out
        // *beside* the strip, or the strip overruns the bar and shoves the native
        // close button off the corner: 12px of `TitleBar` left padding + the three
        // 34px window-control tiles (─ ▢ ✕ = 102px) = 114px. Undershooting it (the
        // old 100px) left the strip ~14px too wide, clipping the "✕".
        let strip_w = if cfg!(target_os = "macos") {
            (window.viewport_size().width - px(80.)).max(px(160.))
        } else {
            (window.viewport_size().width - px(114.)).max(px(140.))
        };
        // Off macOS the bar spans the detail panel (see `app::render`). The corner
        // chrome then sits *over the panel's surface*, so it stops hugging the
        // window controls and aligns with the column it is painted on: a block as
        // wide as the slice of the panel the bar can reach — panel width less the
        // control group, less the panel's own left border — with the tiles packed
        // at its leading edge, on the same inset as everything else in the panel.
        let chrome_band_w = (!cfg!(target_os = "macos") && self.right_panel_open(cx)).then(|| {
            (self.right_panel_px(window, cx) - crate::ui::app::WINDOW_CONTROLS_W - 1.).max(0.)
        });
        // The "+" and the right-edge overflow "⋯" (30px each), their surrounding
        // gaps, and the strip's own left/right padding all live *outside* the
        // clipped chip row — reserve that whole footprint here so the fixed chrome
        // never overflows the strip box (which would eat the "⋯"'s right inset and
        // shove it into the window corner) and cap the chip row at the remainder.
        let chips_avail = (strip_w - px(100.) - px(chrome_band_w.unwrap_or(0.))).max(px(80.));
        // Only the chip row clips; a crowded row shrinks its chips (down to their
        // `min_w`) and truncates their labels rather than pushing the "+" away.
        let mut chips = h_flex()
            .items_center()
            .gap(px(CHIP_GAP))
            .min_w_0()
            .max_w(chips_avail)
            .overflow_hidden();

        // ── Live drag-reorder ─────────────────────────────────────────────────
        // While a chip is being dragged the strip renders in the order a drop
        // would produce (see [`crate::ui::reorder`]): the dragged chip travels
        // along the row itself — nothing floats over the window — and every
        // chip it passes slides over to meet it. `slots` collects this frame's
        // chip geometry: the reference a drag starting on a later frame freezes.
        let slots: Rc<RefCell<Vec<Bounds<Pixels>>>> =
            Rc::new(RefCell::new(vec![Bounds::default(); self.tabs.len()]));
        let preview = reorder::preview(
            &self.reorder,
            &Surface::Strip,
            self.tabs.len(),
            window.mouse_position(),
        );
        // Display order: plain tab order, or the previewed one mid-drag. In the
        // strip a slot *is* a tab index, so the previewed order doubles as the
        // tab permutation a release would commit — recorded every frame so
        // letting go applies exactly what's on screen (see `reorder`).
        let display: Vec<usize> = match &preview {
            Some(p) => {
                reorder::set_pending(&self.reorder, &Surface::Strip, p.order.clone());
                p.order.clone()
            }
            None => (0..self.tabs.len()).collect(),
        };

        for i in display {
            // In sidebar mode the vertical rail carries the tab list; the strip
            // keeps only its "+"/"⋯" chrome, so skip the chip row entirely.
            if !show_chips {
                break;
            }
            // The chip you're holding: still a chip in the row, just dimmed so
            // it reads as picked up while it slides between slots.
            let dragged = preview.as_ref().is_some_and(|p| p.from == i);
            let tab = &self.tabs[i];
            let is_active = i == active;
            let label = self.tab_label(tab, i, Some(window), cx);
            // SSH status dot (PRD FR-E2): coloured by the pane's connection phase.
            let ssh_dot = self.tab_ssh_dot(tab, cx);
            // A coding agent running in this tab (Claude Code, Codex, …) fronts
            // its chip with the vendor brand mark so it's recognizable at a
            // glance across a crowded strip.
            let agent = tab.agent(cx);
            let agent_status = tab.agent_status(cx);
            let agent_unread = tab.agent_unread_count(cx);

            // Inline rename input for this tab, if it's the one being renamed.
            let rename_input = self
                .renaming
                .as_ref()
                .filter(|r| r.index == i)
                .map(|r| r.input.clone());
            // Either the editable input (while renaming) or the clickable,
            // draggable label.
            let label_region = match rename_input {
                Some(input) => div()
                    .id(("tab-rename", i))
                    .flex_1()
                    .min_w_0()
                    // Swallow mouse-downs (incl. double-click word-select inside
                    // the input) so they never reach the enclosing TitleBar and
                    // zoom/maximize the window.
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(Input::new(&input).appearance(false))
                    .into_any_element(),
                None => div()
                    .id(("tab-label", i))
                    .flex_1()
                    .min_w_0()
                    // Ellipsis-truncate the label so a shrunken chip degrades
                    // gracefully instead of hard-clipping mid-glyph.
                    .truncate()
                    .text_sm()
                    // Active tab carries a hair more weight so hierarchy reads
                    // from the type, not from colour alone.
                    .when(is_active, |d| d.font_weight(FontWeight::MEDIUM))
                    .child(label)
                    // No mouse handler of its own: click and drag both live on
                    // the chip, and a child that swallowed the press would take
                    // the label — most of the chip — out of both.
                    .into_any_element(),
            };

            let chip = h_flex()
                .id(("tab-chip", i))
                // Drag anywhere on the chip to reorder it. On the chip, not on
                // its label: the drag's frame of reference is where the *chip*
                // was grabbed, which is what the frozen geometry below
                // measures — hang it off the label and the held chip rides
                // offset from the cursor, skewing every crossing by that much.
                // The builder runs once, when gpui promotes the press into a
                // drag, freezing the strip's geometry as of the last painted
                // frame.
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
                // The strip lives inside gpui-component's `TitleBar`, which marks
                // its whole area as `WindowControlArea::Drag`. On Windows that maps
                // to `HTCAPTION`, so unless an element on top registers a
                // mouse-blocking hitbox, the OS swallows clicks as window-drags and
                // our `on_mouse_down` never fires. `occlude()` makes the chip a
                // `BlockMouse` hitbox so hit-testing stops here (its label/close
                // children paint above it, so they still click through). No-op on
                // macOS, where titlebar dragging doesn't gate child hit-testing.
                .occlude()
                // A group so this chip's close affordance can reveal on hover
                // (progressive disclosure) without affecting sibling tabs.
                .group(SharedString::from(format!("tab-chip-{i}")))
                // Same as the sidebar rows: a chip is a switch target first, so
                // hover says "click me" and the drag swaps in the closed hand
                // (see `Tty7App::render`).
                .cursor_pointer()
                .items_center()
                .justify_between()
                .gap_1p5()
                .h(px(30.))
                // Content-adaptive width with a readable floor. The three inputs
                // that should decide a chip's width all flow through flexbox: its
                // own label length (the flex basis is the content), the other
                // tabs' lengths, and the window. A short label ("~") sits at the
                // floor; a longer one grows to fit — with no pixel cap on top, so
                // a wide window with few tabs shows labels in full (the only upper
                // bound is `short_title`'s 40-char clamp). When the row overflows,
                // `flex_shrink` trims every chip in proportion to its basis (the
                // longest give up the most) down to `min_w`, which is the shrink
                // floor too — kept modest so a crowded strip stays readable rather
                // than collapsing to slivers, and so plenty of tabs fit first.
                .min_w(px(100.))
                .flex_shrink(1.)
                .pl_3()
                .pr_1p5()
                .rounded_lg()
                // Active tab: a soft lifted fill, no border — reads native
                // (Safari/Arc) rather than as a hard-edged box. Inactive: quiet
                // muted text with a barely-there fill on hover for feedback.
                .when(is_active, |s| {
                    s.bg(cx.theme().secondary).text_color(cx.theme().foreground)
                })
                .when(!is_active, |s| {
                    s.text_color(cx.theme().muted_foreground)
                        .hover(|s| s.bg(cx.theme().muted))
                })
                // Held: a light dimming so the chip under your cursor reads as
                // picked up. Not a lift — it stays in the row's own plane.
                .when(dragged, |s| s.opacity(0.75))
                // Measures this chip into the frame's slot table. Absolute and
                // empty, so it costs the layout nothing.
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
                    // `inset_0`, not `size_full`: an absolutely-positioned
                    // child with no insets is laid out at its parent's
                    // *content* box, so a measuring canvas inside a padded
                    // element would report an origin shifted right by the
                    // left padding — and the held chip would ride that far
                    // off the cursor. Pinning all four insets to 0 anchors it
                    // to the padding box, which is the chip itself.
                    .absolute()
                    .inset_0(),
                )
                // A click anywhere on the chip activates the tab; a double click
                // zooms the window, as the rest of the title bar does. Both live
                // here rather than on the label so the whole chip — label, icon,
                // padding — is one switch target and one drag handle. (The close
                // button and the rename input stop the press for their own use.)
                //
                // The event is swallowed: on Windows the chip's `occlude()` means
                // it would never reach the TitleBar anyway, so the zoom is
                // forwarded explicitly. Caveat: gpui only implements
                // `titlebar_double_click` on macOS; elsewhere it's a no-op until
                // upstream adds support.
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
                // Leading SSH status dot when this tab hosts an SSH session.
                .when_some(ssh_dot, |c, rgb| {
                    c.child(
                        div()
                            .flex_shrink_0()
                            .size(px(6.))
                            .rounded_full()
                            .bg(gpui::rgb(rgb)),
                    )
                })
                // Leading agent brand avatar, when a coding agent runs in this
                // tab — the vendor mark on its accent. Only agents get an avatar
                // here: ordinary shells stay text-only so the strip reads as
                // tabs, not icon-per-chip busy.
                .when_some(agent, |chip, agent| {
                    chip.child(self.tab_avatar(
                        Some(agent),
                        agent_status,
                        agent_unread,
                        None,
                        18.,
                        cx,
                    ))
                })
                // Clickable / editable label region.
                .child(label_region)
                // Trailing ⌘N badge: while the shortcut hints are armed the
                // badge takes an in-flow 20px slot (the strip reflows once as
                // the hints arm/disarm — a deliberate, all-chips-at-once modal
                // moment). It can't float like the close button below: badges
                // also show on unhovered inactive chips, which are transparent
                // over the window background (possibly a gradient or image), so
                // there's no solid colour to back an overlay with.
                .when(show_badges && i < 9, |chip| {
                    // Bare digit, no keycap box — the hint blends into the chip
                    // rather than reading as another button.
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
                // Close affordance: out of flow, so the label runs the full
                // chip width instead of always reserving a 20px slot for a
                // button that's invisible until hover. On hover the ✕ floats
                // over the label's right edge (Safari-style) on a solid backing
                // in the chip's current fill — `secondary` on the active chip,
                // `muted` on an inactive one, whose hover fill is exactly what
                // the ✕'s visibility implies — with a short gradient run-in so
                // covered text fades out instead of hard-cutting mid-glyph.
                // Nothing reflows on hover.
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
                            .top(px(5.))
                            .right(px(6.))
                            .opacity(0.)
                            .group_hover(SharedString::from(format!("tab-chip-{i}")), |s| {
                                s.opacity(1.)
                            })
                            .child(div().w(px(10.)).h(px(20.)).bg(linear_gradient(
                                90.,
                                linear_color_stop(fade_from, 0.),
                                linear_color_stop(backing, 1.),
                            )))
                            .child(
                                div().bg(backing).child(
                                    Button::new(("tab-close", i))
                                        .icon(IconName::Close)
                                        .ghost()
                                        .xsmall()
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.close_tab(i, window, cx);
                                        })),
                                ),
                            ),
                    )
                });

            // Per-tab right-click menu (rename / worktree / split / copy cwd /
            // close group) — the same builder the sidebar rows use.
            let menu_app = cx.entity().downgrade();
            let chip = chip.context_menu(move |menu, window, cx| {
                Self::tab_context_menu(menu, i, false, &menu_app, window, cx)
            });
            chips = chips.child(match &preview {
                // The chip in hand: drawn wherever the cursor is holding it,
                // pixel for pixel, with no animation in the way. `deferred`
                // keeps its slot in the layout but paints it after its
                // siblings, so it passes *over* the chips it's crossing
                // instead of being clipped behind them.
                Some(p) if p.from == i => deferred(chip.relative().left(p.held)).into_any_element(),
                // A chip the drag just crossed starts the frame where it used
                // to be and eases to its new slot. `offset` is zero for every
                // chip the last crossing left alone, so this is one moving
                // chip at a time, not the whole row re-animating every frame.
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

        // "+" new-tab button — click opens the shell picker. The default shell
        // leads the menu (so the common case is two quick clicks on the same
        // spot; ⌘T still opens a default tab in one), followed by every shell
        // discovered on this machine (`detected_shells`, probed at startup).
        // Built on gpui-component's `DropdownMenu`, which is only implemented
        // for `Button` — hence a ghost Button restyled to the title bar's tile
        // rhythm (`TILE_SIZE` box, soft corners) rather than the hand-rolled
        // tile the "+" used to be. `TILE_GLYPH_LINE`, not `TILE_GLYPH`: lucide's
        // "+" draws inside a smaller share of its viewBox than the framed marks
        // beside it, and would otherwise read a fifth small.
        let add_button =
            // Same Windows titlebar note as the chips above: `occlude()` gives
            // the trigger a BlockMouse hitbox so the TitleBar's HTCAPTION drag
            // area doesn't swallow the click.
            div().occlude().flex_shrink_0().child(
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

        // Sidebar mode with the rail collapsed: the rail's own controls move here
        // rather than vanishing with it, so collapsing is never a one-way door.
        // They keep the rail's order and spacing and just re-anchor from the rail's
        // right edge to the window's left one, landing beside the traffic lights.
        let rail_collapsed = !show_chips && !self.left_panel_open(cx);
        let left_group = rail_collapsed.then(|| {
            h_flex()
                .flex_shrink_0()
                .items_center()
                .gap(px(2.))
                // Negative off macOS only: the bar already inset us past the window
                // controls, and there the reserve *is* the clearance.
                .ml(px(crate::ui::app::title_bar_hug_offset()))
                // The brand mark follows the rail's controls into the strip, so
                // collapsing the sidebar doesn't strip the window's leading corner
                // back to nothing (see `app::window_mark`). The group is anchored by
                // its tiles' *hit boxes*, which start `tile_trailing_inset()` from
                // the window edge; the mark has no box, so it adds the difference
                // back to land its own ink on `CONTENT_INSET` like the rail's did.
                .when_some(crate::ui::app::window_mark(), |group, mark| {
                    group.child(
                        div()
                            .flex_shrink_0()
                            .pl(px(crate::ui::app::CONTENT_INSET
                                - crate::ui::app::tile_trailing_inset()))
                            // The mark is solid where the tiles are line work, so it
                            // needs more air than the 2px that separates two tiles
                            // before the "+" beside it stops reading as part of it.
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
                        .tooltip("Show Sidebar")
                        .on_click(cx.listener(|this, _, _window, cx| this.toggle_left_panel(cx))),
                    ),
                )
        });

        let panel_open = self.right_panel_open(cx);
        // The window's right-corner chrome. On macOS, when the panel is open it
        // lives on the *panel's* top zone (the panel is what reaches the window's
        // right edge then) exactly like the rail's controls live on the rail; the
        // strip only carries it while the panel is closed. Off macOS the bar spans
        // the panel (the window controls are at its right end — see `app::render`),
        // so the strip always reaches the right edge and always carries the chrome.
        let right_chrome =
            (!panel_open || !cfg!(target_os = "macos")).then(|| self.window_chrome(window, cx));

        // Outer strip: the clipping chip row and the always-visible "+" anchored
        // left, the overflow "⋯" pushed to the right edge by a flexible spacer.
        // Only `chips` is width-capped and `overflow_hidden`, so neither button is
        // pushed off-screen no matter how many tabs are open.
        h_flex()
            .id("tab-strip")
            .items_center()
            .gap_1p5()
            // Chip mode: viewport-derived width (see `strip_w`) so the right edge —
            // and the "⋯" pinned to it — tracks the window instead of drifting.
            // Sidebar mode: the strip lives in the narrower right column beside the
            // rail, so it just fills that column (`w_full`) and the "⋯" pins to its
            // right; the viewport width would overrun the column and push it off.
            .when(show_chips, |this| this.w(strip_w))
            .when(!show_chips, |this| this.w_full())
            // Padding, not margin: taffy is border-box, so a horizontal *margin*
            // would push the strip past its box and clip the "⋯"; padding stays
            // inside the width. `pr_2` (8px) sets the "⋯"'s gap from the right edge
            // — the original tight inset, which now holds steady on resize since
            // `strip_w` keeps the right edge tracking the window.
            .pl_0()
            .min_w_0()
            .when_some(left_group, |this, g| this.child(g))
            .child(chips)
            // In sidebar mode the rail owns "New Tab" (a "+" in its own top bar),
            // so the title bar drops its "+" to avoid a redundant second one —
            // leaving just the "⋯" overflow menu on a thin strip.
            .when(show_chips, move |this| this.child(add_button))
            .child(div().flex_1())
            .when_some(right_chrome, |this, chrome| match chrome_band_w {
                // Over the panel: left-aligned on the panel's content edge, and
                // wide enough that its own right edge lands where the window
                // controls start.
                Some(w) => this.child(
                    h_flex()
                        .flex_none()
                        .w(px(w))
                        .items_center()
                        .pl(px(tile_trailing_inset()))
                        .child(chrome),
                ),
                // Over the terminal: pinned to the strip's trailing edge, which is
                // the window's right edge (or the controls' left edge off macOS).
                None => this.child(chrome),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_title_strips_user_host_and_shows_shallow_path_in_full() {
        // Up to KEEP_SEGMENTS deep (home `~` counts as one) shows in full.
        assert_eq!(short_title("user@host:~/projects/app"), "~/projects/app");
        assert_eq!(short_title("/usr/local/bin"), "/usr/local/bin");
        assert_eq!(short_title("plain"), "plain");
    }

    #[test]
    fn short_title_truncates_deep_paths_to_trailing_segments() {
        // Deeper than KEEP_SEGMENTS collapses to `…/` plus the last three.
        assert_eq!(short_title("user@host:~/repo/025/tty7"), "…/repo/025/tty7");
        assert_eq!(short_title("/usr/local/share/man"), "…/local/share/man");
        assert_eq!(short_title("a/b/c/d"), "…/b/c/d");
    }

    #[test]
    fn short_title_keeps_home_tilde_and_normalizes_trailing_slash() {
        assert_eq!(short_title("user@host:~"), "~");
        assert_eq!(short_title("~"), "~");
        // Trailing slash is dropped; the path is shown, not just its basename.
        assert_eq!(short_title("a/b/c/"), "a/b/c");
    }

    #[test]
    fn short_title_blank_input_is_empty_and_long_names_are_clamped() {
        assert_eq!(short_title("   "), "");
        let long = "a".repeat(50);
        let out = short_title(&long);
        // Clamp is 40 chars plus a single ellipsis.
        assert_eq!(out.chars().count(), 41);
        assert!(out.ends_with('…'));
    }
}

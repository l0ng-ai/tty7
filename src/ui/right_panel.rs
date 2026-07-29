//! The right detail panel: a docked column showing what the active pane *is*,
//! rather than what it's printing — session facts, its working-tree diff, and
//! its file tree.
//!
//! Its tab row has two homes. On macOS it is the panel's own title-bar-height top
//! zone, level with the window's chrome, so the column runs unbroken from the top
//! of the window. Off macOS the title bar has to span the panel (the window
//! controls live at its right end), so the row drops to the panel's second line —
//! Cursor-style — while the caption row above is painted in the panel's surface
//! so the column still reads as one colour.
//! Either way the tiles themselves are built in
//! [`tab_strip`](crate::ui::tab_strip), beside the rest of the window's tiles.
//!
//! No new source of truth: Info reads the same `TerminalView`/`Tab` accessors the
//! sidebar row does, Changes probes the same `git_diff` the diff overlay does, and
//! Files renders the same rows as the code panel's tree.

use gpui::{AnyElement, Context, Window, div, prelude::*, px};
use gpui_component::button::Button;
use gpui_component::input::Input;
use gpui_component::{
    ActiveTheme as _, Icon, IconName, InteractiveElementExt as _, Sizable as _, h_flex, v_flex,
};
use std::path::PathBuf;

use crate::core::config::{Config, RightPanelTab};
use crate::daemon::protocol::PaneProcs;
use crate::terminal::git_diff::{self, DiffSnapshot};
use crate::ui::app::{
    CONTENT_INSET, TILE_GLYPH_SM, TILE_SIZE_SM, Tty7App, tile_trailing_inset,
    tile_trailing_inset_sm,
};
use crate::ui::scrollbar::with_vertical_scrollbar;

/// Bounds for the panel's width, mirroring the rail's: a floor so the tree never
/// becomes an ellipsis parade, and a ceiling as a fraction of the window so a
/// persisted value can't swallow the terminal.
///
/// The floor is also what has to seat the panel's top row on macOS, which is the
/// binding constraint: four chrome tiles, the panel toggle and the "⋯" — six
/// 32px boxes, five 2px gaps and the two glyph-aligned insets — need **214px**.
/// A tighter floor doesn't make the panel narrower, it makes that row overflow;
/// the alternative (shrinking the tabs to body scale) was tried and reads as the
/// panel's own navigation being demoted below the two buttons beside it. 216
/// leaves the row a hair of slack and is still narrower than any window this
/// panel is usable in.
pub(crate) const MIN_WIDTH: f32 = 216.;
pub(crate) const MAX_WIDTH_RATIO: f32 = 0.5;

/// Width (px) of the resize handle's invisible hit-area, centered on the panel's
/// left border — same geometry as the tab rail's.
const RESIZE_HANDLE_WIDTH: f32 = 8.;

/// Panel state that isn't a user preference (those live in `Config`): the cached
/// diff for the Changes tab and the body's scroll position.
#[derive(Default)]
pub(crate) struct RightPanelState {
    /// The machine and cwd `diff` was probed from — compared against the active
    /// pane's host and cwd to decide whether the cached snapshot is still about
    /// the right repository. The host is half the identity: the same path on two
    /// machines is two repositories.
    pub(crate) diff_cwd: Option<(crate::ui::host_ops::HostId, PathBuf)>,
    /// Last completed probe. `Some(None)` and `None` are different answers:
    /// "probed, not a work tree" versus "never probed".
    pub(crate) diff: Option<Option<DiffSnapshot>>,
    /// A probe is in flight; keeps the render path from spawning a second one.
    pub(crate) diff_loading: bool,
    /// The pane `procs` describes, so a pane switch invalidates it rather than
    /// showing the previous pane's processes under the new pane's name.
    pub(crate) procs_pane: Option<u64>,
    /// Last completed process/port query for `procs_pane`.
    pub(crate) procs: Option<PaneProcs>,
    /// A poll cycle is live — a query is in flight *or* the inter-tick timer is
    /// waiting between ticks. The render path checks this before starting the
    /// loop, so a re-render never starts a second chain. It must stay set across
    /// the timer too: clearing it the instant a query returned let every repaint
    /// in the 2s gap kick off another query, collapsing the interval into a tight
    /// query→notify→repaint→query loop that made the list flicker.
    pub(crate) procs_loading: bool,
    /// Bumped on every pane switch to retire the in-flight poll loop: a tick whose
    /// generation no longer matches drops its result and stops rescheduling, so the
    /// freshly started loop for the new pane is the only one left running.
    pub(crate) procs_gen: u64,
    /// Whether the current pane also wants its SSH forwards re-listed on the
    /// procs tick. Kept here rather than only captured by the running loop
    /// because it can flip *without* a pane switch — a native-SSH pane you are
    /// already watching finishes connecting — and the loop reads this on each
    /// reschedule so it picks the change up on the next tick.
    /// How the Forwards band's requests reach the daemon while this poll loop
    /// runs, or `None` when the pane on screen has nothing to forward over.
    ///
    /// A route rather than a `bool` because a remote workspace's forwards belong
    /// to the *workspace*, not the pane (design §15): the pane id alone cannot
    /// say which of the two owners to ask, and the reschedule below re-reads
    /// this rather than carrying the decision forward.
    pub(crate) procs_forwards: Option<crate::ui::app::ForwardRoute>,
    /// Scroll position of the shared body container (Info / Outline / Changes),
    /// owned here rather than left to gpui's element-id state so the overlay
    /// scrollbar has a handle to read the offset from and to drag.
    pub(crate) scroll: gpui::ScrollHandle,
    /// The Files tab's local tree scrolls in its own container (it carries the
    /// tree's focus handle and key bindings), so it needs its own handle.
    pub(crate) tree_scroll: gpui::ScrollHandle,
}

/// How often the Info tab re-queries processes and ports while it's open. Fast
/// enough that starting a dev server shows up as you tab over, slow enough that
/// the process-table walk stays off the profile.
const PROCS_POLL: std::time::Duration = std::time::Duration::from_millis(2000);

impl Tty7App {
    /// Whether the right panel is docked open. The title bar's tab row, the body
    /// column and the code overlay's right inset all derive from this.
    pub(crate) fn right_panel_open(&self, _cx: &gpui::App) -> bool {
        self.right_panel_visible && !self.tabs.is_empty()
    }

    /// The panel's live width, re-clamped to the window the same way the rail's
    /// is, so a persisted value from a larger display can't take over.
    /// Named `_px` rather than `_width` because the field it reads is
    /// `right_panel_width`; a method of the same name would shadow it awkwardly
    /// at every call site.
    pub(crate) fn right_panel_px(&self, window: &Window, _cx: &gpui::App) -> f32 {
        let max = (window.viewport_size().width.as_f32() * MAX_WIDTH_RATIO).max(MIN_WIDTH);
        // The live cell, not the config: a drag in progress writes only here, and
        // persists to the config on release.
        self.right_panel_width.get().clamp(MIN_WIDTH, max)
    }

    /// `ToggleRightPanel` (⌘J). Flips this window's panel; the config write is
    /// only what the *next* window will start with — see the field's doc comment.
    pub(crate) fn toggle_right_panel(&mut self, cx: &mut Context<Self>) {
        let next = !self.right_panel_visible;
        self.right_panel_visible = next;
        self.update_config(cx, |cfg| cfg.right_panel_visible = next);
        cx.notify();
    }

    /// Select a tab. Opens the panel if it was closed, so the title bar's tab
    /// tiles double as "show me this" rather than being inert while hidden.
    pub(crate) fn set_right_panel_tab(&mut self, tab: RightPanelTab, cx: &mut Context<Self>) {
        self.right_panel_tab = tab;
        self.right_panel_visible = true;
        self.update_config(cx, |cfg| {
            cfg.right_panel_tab = tab;
            cfg.right_panel_visible = true;
        });
        cx.notify();
    }

    /// The docked column, or `None` while the panel is closed.
    pub(crate) fn render_right_panel(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let panel_open = self.right_panel_open(cx);
        // The remote browser follows the detail pane on *every* paint, not only
        // while Files is on screen. Opening it is the Files tab's job (no point
        // listing a directory nobody asked to see), but retiring it can't be:
        // the transfers footer below is pane-scoped and rides under all four
        // tabs, so a pane switch made from Info has to drop the old pane's
        // browser too — otherwise the footer would report a transfer belonging
        // to a pane you're no longer looking at.
        //
        // This runs *before* the closed-panel bail, and treats a closed panel as
        // "not looking at that pane": the browser owns a 500ms transfer poll that
        // only ends when the browser does, so leaving it open behind a closed
        // panel would keep a daemon round-trip (and a full re-render) running
        // twice a second for a column nobody can see. The poll loop makes the
        // same check on its own tick (`sftp_start_polling`) so its lifetime
        // doesn't rest on this function being called every frame; retiring here
        // as well just gets it done a frame sooner instead of up to 500ms later.
        if let Some(open) = self.sftp_panel.open_pane_id
            && (!panel_open || self.remote_files_pane(window, cx).map(|(id, _)| id) != Some(open))
        {
            self.sftp_close_browser(cx);
        }
        if !panel_open {
            return None;
        }
        let width = self.right_panel_px(window, cx);
        let tab = self.right_panel_tab;

        let body = match tab {
            RightPanelTab::Info => self.render_panel_info(window, cx),
            RightPanelTab::Outline => self.render_panel_outline(window, cx),
            RightPanelTab::Changes => self.render_panel_changes(window, cx),
            RightPanelTab::Files => self.render_panel_files(window, cx),
        };
        let (backing, handle) = self.right_panel_resize(cx);

        Some(
            v_flex()
                .id("right-panel")
                .relative()
                .flex_none()
                .w(px(width))
                .h_full()
                .child(backing)
                // The sunk sidebar surface, like the tab rail: both are chrome
                // around the terminal, so they read as the same material.
                .bg(cx.theme().sidebar)
                .border_l_1()
                .border_color(cx.theme().sidebar_border)
                // A title-bar-height top zone of its own, exactly like the rail's.
                // This is what makes the panel read as one column instead of a box
                // bolted under the title bar: its surface runs the full height of
                // the window, and the tab row sits *on* it rather than on the
                // terminal's bar above a seam.
                //
                // macOS only. Off macOS the bar spans the panel — it has to, or the
                // window controls end up stranded mid-window (see `app::render`) —
                // and a row of tiles under that caption row was one chrome row too
                // many: the panel opened with three stacked headers (caption chrome,
                // tab tiles, section title) before any content. So there the tiles
                // move into the section header instead (`panel_title`), which is a
                // row the panel was drawing anyway.
                .children(cfg!(target_os = "macos").then(|| {
                    let row = h_flex()
                        .id("right-panel-titlebar-drag")
                        .flex_none()
                        .h(px(crate::ui::app::TITLE_BAR_HEIGHT))
                        // gpui-component's `TitleBar` centres its content inside a
                        // `border_b_1` box — border-box shrinks the content height
                        // by that 1px, nudging its centred glyphs up half a pixel.
                        // The corner chrome (⋯, panel toggle) lives in *both* the
                        // title bar and here, so mirror that hidden border to keep
                        // its centre line identical; without it the glyphs jump
                        // down a physical pixel the moment the panel opens.
                        .border_b_1()
                        .border_color(cx.theme().transparent);
                    // The top zone sits level with the real `TitleBar`, but the
                    // bar only spans the terminal column — so, exactly like the
                    // rail's top strip (`tab_sidebar`), make this one act like
                    // the title bar it aligns with: drag to move, double-click
                    // to zoom. A press arms a flag and the first *move* starts
                    // the window move, so a plain click on a tab — and a
                    // double-click — still lands intact; the tabs and corner
                    // chrome take their own. `window_move_gesture` holds that
                    // flag in element state, so a repaint between the press and
                    // the first move can't disarm it (#221).
                    crate::ui::app::window_move_gesture(
                        row,
                        "right-panel-titlebar-drag",
                        window,
                        cx,
                    )
                    .on_double_click(|_, window, _| window.titlebar_double_click())
                    .items_center()
                    .gap(px(2.))
                    // Chrome scale, like the corner controls this row ends with
                    // (`right_panel_tabs`): the leading inset lines the *glyph*
                    // up on `CONTENT_INSET`, so it subtracts the 32px tile's own
                    // padding rather than a 24px one's.
                    .pl(px(tile_trailing_inset()))
                    .children(self.right_panel_tabs(cx))
                    .child(div().flex_1())
                    // The panel is what reaches the window's right edge while
                    // it's open, so it carries the corner chrome.
                    .child(self.window_chrome(window, cx))
                }))
                .child(body)
                // The transfers footer is a sibling of the body, not part of any
                // tab: an SFTP transfer belongs to the pane, so reading Info or
                // Changes must not make a running upload vanish.
                .children(self.sftp_transfers_footer(cx))
                .child(handle)
                .into_any_element(),
        )
    }

    /// The panel's resize drag: a measuring canvas that installs window-level
    /// mouse listeners while held, plus the handle itself. Mirrors the tab rail's
    /// (`tab_sidebar.rs`) with the axis flipped — this panel is anchored to the
    /// window's right edge, so width grows as the pointer moves *left*, measured
    /// from the panel's own right edge rather than its origin.
    fn right_panel_resize(&self, cx: &mut Context<Self>) -> (AnyElement, AnyElement) {
        use gpui::{Bounds, MouseButton, MouseMoveEvent, MouseUpEvent, Pixels, canvas};
        use std::cell::Cell as StdCell;
        use std::rc::Rc;

        let container: Rc<StdCell<Option<Bounds<Pixels>>>> = Rc::new(StdCell::new(None));
        let backing = canvas(
            {
                let container = container.clone();
                move |bounds, _window, _cx| container.set(Some(bounds))
            },
            {
                let container = container.clone();
                let width_cell = self.right_panel_width.clone();
                let dragging = self.right_panel_dragging.clone();
                move |_bounds, _state, window, _cx| {
                    window.on_mouse_event({
                        let container = container.clone();
                        let width_cell = width_cell.clone();
                        let dragging = dragging.clone();
                        move |ev: &MouseMoveEvent, _phase, window, _cx| {
                            if !dragging.get() {
                                return;
                            }
                            let Some(b) = container.get() else {
                                return;
                            };
                            let right = b.origin.x + b.size.width;
                            let raw = (right - ev.position.x).as_f32();
                            let max = (window.viewport_size().width.as_f32() * MAX_WIDTH_RATIO)
                                .max(MIN_WIDTH);
                            width_cell.set(raw.clamp(MIN_WIDTH, max));
                            window.refresh();
                        }
                    });
                    window.on_mouse_event({
                        let width_cell = width_cell.clone();
                        let dragging = dragging.clone();
                        move |_ev: &MouseUpEvent, _phase, window, cx| {
                            if !dragging.get() {
                                return;
                            }
                            dragging.set(false);
                            let w = width_cell.get();
                            let cfg = cx.global_mut::<Config>();
                            if cfg.right_panel_width != w {
                                cfg.right_panel_width = w;
                                cfg.save();
                            }
                            window.refresh();
                        }
                    });
                }
            },
        )
        .absolute()
        .size_full()
        .into_any_element();

        // `occlude()` for the same reason as the rail's handle (`tab_sidebar`):
        // it spans the panel's full height, so its top band lies over a
        // `WindowControlArea::Drag` row — the macOS top zone, and `panel_title`
        // below it — and a non-blocking hitbox lets a press arm that row's window
        // move alongside the resize.
        let active = self.right_panel_dragging.get();
        let handle = div()
            .group("right-panel-resize")
            .occlude()
            .absolute()
            .top_0()
            .left(px(-(RESIZE_HANDLE_WIDTH / 2.)))
            .w(px(RESIZE_HANDLE_WIDTH))
            .h_full()
            .flex()
            .items_center()
            .justify_center()
            .cursor_col_resize()
            .child(
                div()
                    .w(px(1.))
                    .h_full()
                    .when(active, |d| d.bg(cx.theme().drag_border))
                    .group_hover("right-panel-resize", |s| s.bg(cx.theme().drag_border)),
            )
            .on_mouse_down(MouseButton::Left, {
                let dragging = self.right_panel_dragging.clone();
                move |_ev, window, _cx| {
                    dragging.set(true);
                    window.refresh();
                }
            })
            .into_any_element();

        (backing, handle)
    }

    /// A tab's header: the name in a weightier small-caps than the old faint
    /// label, plus an optional live count trailing it (files, commands, changed
    /// files) so the header states scale at a glance, and an optional control on
    /// the right. The count is the quiet mono tally the sidebar group headers use.
    /// `trailing` carries a tab's own controls where it has any, so they sit on
    /// the label's line rather than earning a second header row.
    ///
    /// Off macOS this row is also the panel's tab switcher: the four tiles ride
    /// at its trailing edge, and the row takes the full title-bar height with a
    /// hairline under it. The panel there hangs below a caption row that already
    /// carries chrome (see `render_right_panel`), and a tile row of its own on top
    /// of this one meant three stacked headers before a single line of content —
    /// so the two that were saying "this is a header" merge into one that also
    /// says which tab you are on.
    ///
    /// **On macOS it draws nothing unless a tab passes `trailing`.** The panel
    /// there has its own tile row in its top zone, which already says which tab
    /// you are on — restating it in words underneath was a whole row spent on
    /// something the selected tile and the content below both already answer
    /// (a file tree is Files, a diff is Changes). What it did cost was the row:
    /// the panel opened with tiles, then a title, then a search box, before one
    /// line of content. The counts it used to carry move into the tab tooltips.
    /// A tab that has its own control still gets the row, because that control
    /// has nowhere else to go.
    ///
    /// When it *does* draw, it is grabbable, like every header in the window (see
    /// [`crate::ui::app::window_move_gesture`]): it sits above the panel's scroll
    /// container, not inside it, so a drag here has nothing else to mean. The
    /// label and its count take no hit box, so the row stays grabbable straight
    /// through them however long the text gets — the same rule the "duo" mark
    /// established in #202. Anything in `trailing` is a control and must carry
    /// its own `occlude()`, or Windows' HTCAPTION eats its clicks. The gesture is
    /// armed *after* the empty-row early return, so the macOS zero-height case
    /// never becomes an invisible drag area.
    pub(crate) fn panel_title(
        &self,
        text: &str,
        count: Option<String>,
        trailing: Option<AnyElement>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tabs = (!cfg!(target_os = "macos")).then(|| self.right_panel_tabs(cx));
        let has_trailing = trailing.is_some();
        if tabs.is_none() && !has_trailing {
            return div().flex_none().into_any_element();
        }
        let row = crate::ui::app::window_move_gesture(
            h_flex().id("panel-title"),
            "panel-title-drag",
            window,
            cx,
        );
        row.flex_none()
            // Tall enough to seat the chrome-scale tiles when it carries them;
            // otherwise the compact label line it has always been.
            .h(px(if tabs.is_some() {
                crate::ui::app::TITLE_BAR_HEIGHT
            } else {
                32.
            }))
            .items_center()
            .pl(px(CONTENT_INSET))
            // Trailing tiles align on the glyph like every other control in the
            // window; a label-only header just takes the plain inset. `_SM` for a
            // tab's own control, whose glyph sits a different distance inside its
            // box than the chrome-scale tab tiles do.
            .pr(px(match (&tabs, has_trailing) {
                (Some(_), _) => tile_trailing_inset(),
                (None, true) => tile_trailing_inset_sm(),
                (None, false) => CONTENT_INSET,
            }))
            // The line that separates the header from the tab's content. Only
            // where the header is the switcher: a label alone doesn't need ruling
            // off from the band it introduces.
            .when(tabs.is_some(), |this| {
                this.border_b_1().border_color(cx.theme().sidebar_border)
            })
            .child(
                h_flex()
                    .flex_shrink_0()
                    .items_baseline()
                    .gap(px(7.))
                    .child(
                        div()
                            .text_size(px(11.5))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(cx.theme().secondary_foreground)
                            .child(text.to_uppercase()),
                    )
                    .when_some(count, |this, c| {
                        this.child(
                            div()
                                .text_size(px(11.))
                                .font_family(cx.theme().mono_font_family.clone())
                                .text_color(cx.theme().muted_foreground.opacity(0.75))
                                .child(c),
                        )
                    }),
            )
            .child(div().flex_1().min_w_0())
            .when_some(trailing, |this, t| this.child(t))
            .when_some(tabs, |this, tiles| {
                this.child(
                    h_flex()
                        .flex_shrink_0()
                        .items_center()
                        .gap(px(2.))
                        // Clear of a tab's own control where there is one; flush
                        // against the label's spring where there isn't.
                        .when(has_trailing, |this| this.ml(px(6.)))
                        .children(tiles),
                )
            })
            .into_any_element()
    }

    /// A tab's filter box — the same borderless magnifier + input the tab rail
    /// uses, so everything in the window searches the same way. Sits under the
    /// header rather than in it: it's a full-width control, not a trailing tile.
    /// Takes the input so the local tree and the remote browser can each keep
    /// their own query while sharing the one appearance.
    pub(crate) fn panel_search(
        &self,
        input: &gpui::Entity<gpui_component::input::InputState>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        h_flex()
            .flex_none()
            .items_center()
            .gap(px(8.))
            .h(px(30.))
            .px(px(CONTENT_INSET))
            .child(
                Icon::new(IconName::Search)
                    .small()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(Input::new(input).appearance(false).xsmall()),
            )
            .into_any_element()
    }

    /// The body's scrolling area, so every tab shares one scroll container and
    /// one content inset.
    fn panel_scroll(&self, inner: AnyElement, title: AnyElement) -> AnyElement {
        let body = div()
            .id("right-panel-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.right_panel.scroll)
            .child(inner);
        v_flex()
            .flex_1()
            .min_h_0()
            .child(title)
            .child(with_vertical_scrollbar(
                "right-panel-body-scrollbar",
                body,
                &self.right_panel.scroll,
            ))
            .into_any_element()
    }

    /// A quiet "nothing to show" line, used wherever a tab has no data yet,
    /// with an optional second line saying what would fill it.
    ///
    /// The hint is the point. An empty state that only reports the absence
    /// ("No changes.") leaves the user to work out whether the panel is broken,
    /// still loading, or simply pointed at the wrong thing; one that names the
    /// condition turns a dead end into an instruction.
    fn panel_empty(&self, text: &str, hint: Option<&str>, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        v_flex()
            .px(px(CONTENT_INSET))
            .py(px(4.))
            .gap(px(3.))
            .text_size(px(12.))
            .text_color(muted)
            .child(text.to_string())
            .children(hint.map(|h| {
                div()
                    .text_size(px(11.))
                    .text_color(muted.opacity(0.75))
                    .child(h.to_string())
            }))
            .into_any_element()
    }

    // ── Info ────────────────────────────────────────────────────────────────

    /// Session facts for the active pane, as a two-column key/value list. Every
    /// row comes from an accessor the sidebar already uses, so the panel can
    /// never disagree with the row that spawned it.
    fn render_panel_info(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let title = self.panel_title("Info", None, None, window, cx);
        let mut rows: Vec<(&'static str, String)> = Vec::new();
        // Held aside from `rows` because they're not key/value lines: the actions
        // hang off the cwd, and the two lists get their own sub-headers below.
        let mut cwd_for_actions: Option<PathBuf> = None;
        let mut pane_id: Option<u64> = None;
        // Set only for a *connected native* SSH pane — the one kind that can carry
        // forwards. A foreground `ssh` typed into a local shell has no connection
        // to forward over, and a still-connecting one has nothing to list yet.
        let mut forwards_pane: Option<u64> = None;

        if let Some(tab) = self.tabs.get(self.active) {
            if let Some(leaf) = tab.detail_pane(window, cx) {
                let view = leaf.read(cx);
                pane_id = Some(view.pane_id);
                if let Some(cwd) = view
                    .git_status_cwd()
                    .map(|p| p.to_path_buf())
                    .or_else(|| view.cwd())
                {
                    rows.push(("cwd", compact_path(&cwd)));
                    cwd_for_actions = Some(cwd);
                }
                let shell = view.shell_spec().map(|s| s.program.clone());
                rows.push((
                    "shell",
                    crate::core::shells::default_shell_name(shell.as_deref()),
                ));
                if let Some(ssh) = view.ssh_spec() {
                    rows.push(("ssh", ssh.host.clone()));
                }
                // Two ways a pane has something to forward over: it *is* an
                // SSH session, or it belongs to a remote workspace, whose
                // forwards run on the workspace's own connection (design §15).
                // The second arm is empty in this build — nothing binds a pane
                // to a workspace yet — which is deliberate: the band stays
                // empty rather than offering an add that would have nowhere to
                // go.
                let connected_ssh = view
                    .remote_context()
                    .is_some_and(|c| c.kind == crate::daemon::protocol::RemoteKind::NativeSsh)
                    && matches!(
                        view.ssh_phase(),
                        Some(crate::daemon::protocol::SshPhase::Connected)
                    );
                if connected_ssh || view.workspace().is_some() {
                    forwards_pane = Some(view.pane_id);
                }
            }
            if let Some(git) = tab.git_status(Some(window), cx) {
                rows.push(("branch", git.branch.clone()));
                rows.push(("changes", format!("+{} −{}", git.added, git.removed)));
            }
            if let Some(agent) = tab.agent(cx) {
                let name = agent.display_name();
                let status = match tab.agent_status(cx) {
                    Some(s) => format!("{name} · {}", agent_status_label(s)),
                    None => name.to_string(),
                };
                rows.push(("agent", status));
            }
        }

        if rows.is_empty() {
            return self.panel_scroll(
                self.panel_empty(
                    "No active session.",
                    Some("Open a tab to see its shell, directory, and processes here."),
                    cx,
                ),
                title,
            );
        }

        // Keep the process/port query pointed at the pane on screen, and keep it
        // ticking while this tab is the one being looked at. The same tick carries
        // the pane's forwards when it has any to carry.
        let route = forwards_pane.map(|id| self.forward_route(id, cx));
        self.sync_procs(pane_id, route, cx);

        let mono = cx.theme().mono_font_family.clone();
        let mut list = v_flex().px(px(CONTENT_INSET)).py(px(2.)).gap(px(3.));
        for (k, v) in rows {
            list = list.child(
                h_flex()
                    .items_baseline()
                    .gap(px(9.))
                    .py(px(1.))
                    .text_size(px(12.))
                    .child(
                        div()
                            .flex_none()
                            .w(px(46.))
                            .text_color(cx.theme().muted_foreground)
                            .child(k),
                    )
                    .child(
                        // The value is the datum — a path, a branch, a host, a
                        // count — so it takes the mono face, set apart from the
                        // sans key beside it.
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_family(mono.clone())
                            .text_color(cx.theme().foreground)
                            .child(v),
                    ),
            );
        }

        let inner = v_flex()
            // Three labelled bands — Session / Processes / Ports — instead of one
            // flat column, so the pane's facts, what it's running, and what it's
            // listening on read as distinct groups.
            .child(self.panel_subtitle("Session", false, None, cx))
            .child(list)
            .when_some(cwd_for_actions, |this, cwd| {
                this.child(self.cwd_actions(cwd, cx))
            })
            .children(self.procs_section(pane_id, cx))
            .children(self.ports_section(pane_id, cx))
            // Ports says what this pane listens on locally; Forwards says what it
            // routes across the connection. Same family of fact, so it reads as
            // the band after it rather than a feature bolted on.
            .children(self.forwards_section(forwards_pane, cx))
            .into_any_element();
        self.panel_scroll(inner, title)
    }

    /// The "open this cwd in…" row under the Info list. Deliberately only the
    /// destinations that need no configuration — a system reveal and the
    /// clipboard. An "open in $EDITOR" button would need a picker, a stored
    /// choice and a settings page to change it; that's a feature, not a row.
    fn cwd_actions(&self, cwd: PathBuf, cx: &mut Context<Self>) -> AnyElement {
        let reveal_label = reveal_label();
        h_flex()
            .gap(px(2.))
            .px(px(tile_trailing_inset_sm()))
            .pt(px(6.))
            .child(
                crate::ui::tab_strip::chrome_tile_sized(
                    Button::new("panel-info-reveal").icon(Icon::new(IconName::FolderOpen)),
                    TILE_SIZE_SM,
                    TILE_GLYPH_SM,
                    false,
                    cx,
                )
                .rounded_md()
                .tooltip(reveal_label)
                .on_click({
                    let cwd = cwd.clone();
                    move |_, _window, cx| cx.reveal_path(&cwd)
                }),
            )
            .child(
                crate::ui::tab_strip::chrome_tile_sized(
                    Button::new("panel-info-copy-path").icon(Icon::new(IconName::Copy)),
                    TILE_SIZE_SM,
                    TILE_GLYPH_SM,
                    false,
                    cx,
                )
                .rounded_md()
                .tooltip("Copy Path")
                .on_click(move |_, _window, cx| {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                        cwd.display().to_string(),
                    ));
                }),
            )
            .into_any_element()
    }

    /// A small-caps band label inside a tab's body, for the sub-lists that hang
    /// off the Info tab. Lighter than [`panel_title`], which is the tab's own
    /// header. `divider` draws a hairline above it, so the second and third bands
    /// separate from the one before; the first band passes `false`. `trailing`
    /// carries a band's own control where it has one — the same slot
    /// [`panel_title`](Self::panel_title) gives a tab, so a band's `+` sits on its
    /// label's line instead of earning a row.
    pub(crate) fn panel_subtitle(
        &self,
        text: &str,
        divider: bool,
        trailing: Option<AnyElement>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        h_flex()
            .when(divider, |d| {
                d.mt(px(6.)).border_t_1().border_color(cx.theme().border)
            })
            .items_center()
            .justify_between()
            .pl(px(CONTENT_INSET))
            // A trailing tile aligns on its glyph, not its hit box — same
            // correction the tab header makes.
            .pr(px(if trailing.is_some() {
                CONTENT_INSET - crate::ui::app::TILE_PAD
            } else {
                CONTENT_INSET
            }))
            // A tile is 24px tall against a ~15px label, so the band's own top
            // padding would push its glyph off the label's line; give the padding
            // back as a shorter lead when one is present.
            .pt(px(match (divider, trailing.is_some()) {
                (true, false) => 12.,
                (true, true) => 8.,
                (false, false) => 10.,
                (false, true) => 6.,
            }))
            .pb(px(if trailing.is_some() { 0. } else { 4. }))
            .child(
                div()
                    .text_size(px(10.5))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(cx.theme().muted_foreground)
                    .child(text.to_uppercase()),
            )
            .when_some(trailing, |this, t| this.child(t))
            .into_any_element()
    }

    /// The pane's process tree, indented by depth. Returns nothing at all when
    /// the pane is just a shell sitting at its prompt: a one-row "processes"
    /// section that always says `zsh` is a header earning its keep zero times.
    fn procs_section(&self, pane_id: Option<u64>, cx: &mut Context<Self>) -> Option<AnyElement> {
        let procs = &self.procs(pane_id)?.procs;
        if procs.len() < 2 {
            return None;
        }
        let mono = cx.theme().mono_font_family.clone();
        let mut list = v_flex().px(px(CONTENT_INSET)).py(px(1.)).gap(px(2.));
        for p in procs {
            list = list.child(
                h_flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            // Indent by depth so the tree reads without drawing
                            // connector glyphs into a 260px column.
                            .pl(px(f32::from(p.depth) * 10.))
                            .text_size(px(12.))
                            .font_family(mono.clone())
                            .text_color(if p.foreground {
                                cx.theme().foreground
                            } else {
                                cx.theme().muted_foreground
                            })
                            .child(p.name.clone()),
                    )
                    .child(info_chip(
                        &p.pid.to_string(),
                        cx.theme().accent,
                        cx.theme().muted_foreground,
                        &mono,
                    )),
            );
        }
        Some(
            v_flex()
                .child(self.panel_subtitle("Processes", true, None, cx))
                .child(list)
                .into_any_element(),
        )
    }

    /// TCP ports the pane's processes are listening on — the answer to "what
    /// port did that dev server pick?", next to the pane that started it.
    fn ports_section(&self, pane_id: Option<u64>, cx: &mut Context<Self>) -> Option<AnyElement> {
        let ports = &self.procs(pane_id)?.ports;
        if ports.is_empty() {
            return None;
        }
        let mono = cx.theme().mono_font_family.clone();
        let mut list = v_flex().px(px(CONTENT_INSET)).py(px(1.)).gap(px(2.));
        for p in ports {
            list = list.child(
                h_flex()
                    .items_center()
                    .gap(px(8.))
                    .child(info_chip(
                        &p.port.to_string(),
                        cx.theme().accent,
                        cx.theme().foreground,
                        &mono,
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.))
                            .font_family(mono.clone())
                            .text_color(cx.theme().muted_foreground)
                            .child(p.name.clone()),
                    ),
            );
        }
        Some(
            v_flex()
                .child(self.panel_subtitle("Ports", true, None, cx))
                .child(list)
                .into_any_element(),
        )
    }

    /// The cached query, but only when it describes `pane_id` — the pane the
    /// Info tab is currently rendering. `sync_procs` already drops the answer on
    /// a pane switch, so this is belt-and-braces; without the argument the doc
    /// claimed a guarantee the body didn't actually make.
    fn procs(&self, pane_id: Option<u64>) -> Option<&PaneProcs> {
        (pane_id.is_some() && self.right_panel.procs_pane == pane_id)
            .then_some(self.right_panel.procs.as_ref())?
    }

    /// Point the process query at `pane_id` and make sure the poll is running.
    /// Called from the Info tab's render, so the loop starts when the tab is
    /// looked at and dies when it isn't — see [`spawn_procs_query`].
    ///
    /// `forwards` asks the same tick to re-list the pane's SSH forwards. It rides
    /// this loop rather than owning one because it wants the identical lifetime
    /// (Info on screen, this pane) and because a forward can change state without
    /// the UI touching it — a remote bind that loses its listener goes to `Error`
    /// on the daemon, and only a re-list finds out. Off for a non-SSH pane, so a
    /// local shell doesn't pay for a round-trip that can only answer "none".
    ///
    /// It's recorded on the state as well as passed down because it can flip
    /// while the loop is already running — a pane you're watching on Info
    /// finishes connecting, and neither the pane id nor the generation changes,
    /// so nothing would otherwise tell the loop to start asking.
    fn sync_procs(
        &mut self,
        pane_id: Option<u64>,
        forwards: Option<crate::ui::app::ForwardRoute>,
        cx: &mut Context<Self>,
    ) {
        let Some(pane_id) = pane_id else { return };
        self.right_panel.procs_forwards = forwards.clone();
        if self.right_panel.procs_pane != Some(pane_id) {
            self.right_panel.procs_pane = Some(pane_id);
            // Drop the previous pane's answer rather than showing it under the new
            // pane's heading until the first tick lands.
            self.right_panel.procs = None;
            // Same for the forwards: the list is one pane's, and the rows filter by
            // pane id anyway, so leaving the old pane's in place would only flash
            // them under the new pane's band until the tick lands.
            self.loopback_panel.managed.clear();
            // Retire the old pane's loop and free the guard so the new pane's loop
            // can start below; the retired tick bows out on the generation check.
            self.right_panel.procs_gen += 1;
            self.right_panel.procs_loading = false;
        }
        if !self.right_panel.procs_loading {
            self.right_panel.procs_loading = true;
            let generation = self.right_panel.procs_gen;
            self.spawn_procs_query(pane_id, generation, forwards, cx);
        }
    }

    /// One query, then reschedule — the poll loop. It reschedules only while the
    /// panel is open on Info, so the loop is self-terminating: close the panel or
    /// switch tabs and the next completion simply doesn't queue another.
    fn spawn_procs_query(
        &mut self,
        pane_id: u64,
        generation: u64,
        forwards: Option<crate::ui::app::ForwardRoute>,
        cx: &mut Context<Self>,
    ) {
        // `procs_loading` is set by the caller (`sync_procs`) and deliberately
        // stays set across the whole cycle, including the timer wait below.
        cx.spawn(async move |this, cx| {
            // Both round-trips on the one background hop, so the tick costs one
            // scheduling slot rather than two.
            let route = forwards.clone();
            let (procs, managed) = cx
                .background_executor()
                .spawn(async move {
                    let procs = crate::terminal::RemoteTerminal::query_procs(pane_id);
                    let managed = route.map(|r| r.list()).unwrap_or_default();
                    (procs, managed)
                })
                .await;
            let keep_polling = this
                .update(cx, |app, cx| {
                    // A pane switch while we flew bumped the generation: drop this
                    // answer and leave the guard to whoever owns the new one.
                    if app.right_panel.procs_gen != generation {
                        return false;
                    }
                    app.right_panel.procs = Some(procs);
                    if forwards.is_some() {
                        app.loopback_panel.managed = managed;
                    }
                    cx.notify();
                    // This window's own panel state, not the config's: another
                    // window closing its panel must not stop our poll.
                    let wanted =
                        app.right_panel_visible && app.right_panel_tab == RightPanelTab::Info;
                    if !wanted {
                        // Loop ends here; release the guard so reopening restarts it.
                        app.right_panel.procs_loading = false;
                    }
                    wanted
                })
                .unwrap_or(false);
            if !keep_polling {
                return;
            }
            cx.background_executor().timer(PROCS_POLL).await;
            let _ = this.update(cx, |app, cx| {
                // Re-check rather than trusting the pre-sleep decision: two seconds
                // is plenty of time to switch panes or close the panel.
                if app.right_panel.procs_gen != generation {
                    return;
                }
                let wanted = app.right_panel_visible && app.right_panel_tab == RightPanelTab::Info;
                if wanted {
                    // Re-read rather than carrying the flag forward: the pane may
                    // have finished connecting since this cycle started, which is
                    // the one way it changes without a pane switch to retire us.
                    let forwards = app.right_panel.procs_forwards.clone();
                    app.spawn_procs_query(pane_id, generation, forwards, cx);
                } else {
                    app.right_panel.procs_loading = false;
                }
            });
        })
        .detach();
    }

    // ── Outline ─────────────────────────────────────────────────────────────

    /// The pane's commands, newest first, each scrolling the terminal back to
    /// where it ran. Positions come from the OSC 133 marks the reader thread
    /// records — see [`crate::terminal::marks`].
    ///
    /// Newest first because that's the end you came from: you scrolled past the
    /// thing you want, and the list should start where your attention is.
    fn render_panel_outline(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        // This panel is a sunk rail (see the `sidebar` fill on its container), so
        // its rows read the sidebar ladder.
        let sf = cx.global::<crate::ui::presets::Surfaces>().sidebar;
        let Some(leaf) = self
            .tabs
            .get(self.active)
            .and_then(|t| t.detail_pane(window, cx))
        else {
            let title = self.panel_title("Outline", None, None, window, cx);
            return self.panel_scroll(
                self.panel_empty(
                    "No active session.",
                    Some("Open a tab to see its shell, directory, and processes here."),
                    cx,
                ),
                title,
            );
        };
        // Count first (a cheap getter) so the borrow ends before `panel_title`
        // needs `&mut cx`; the list re-borrows the marks below.
        let count = leaf.read(cx).command_marks().len();
        if count == 0 {
            // Two very different causes, one honest sentence: nothing has run
            // yet, or this shell never reported OSC 133 (no integration, a bare
            // `sh`, a nested PTY that eats the marks).
            let title = self.panel_title("Outline", None, None, window, cx);
            return self.panel_scroll(
                self.panel_empty(
                    "No commands recorded for this pane.",
                    Some("Run a command — shell integration marks each one so you can jump back to it."),
                    cx,
                ),
                title,
            );
        }
        let title = self.panel_title("Outline", Some(count.to_string()), None, window, cx);

        let mono = cx.theme().mono_font_family.clone();
        let mut list = v_flex().px(px(CONTENT_INSET - 4.)).py(px(2.)).gap(px(1.));
        let marks = leaf.read(cx).command_marks();
        for mark in marks.iter().rev() {
            let row = mark.row;
            let leaf = leaf.clone();
            let failed = mark.exit.is_some_and(|c| c != 0);
            let running = !mark.done;
            // A leading status marker reads as a shape first: a hollow ring for a
            // clean finish, a filled dot while it runs, and — the only tinted one
            // — a danger dot for a nonzero exit. The failure is what you scan for.
            let dot = {
                let d = div().flex_none().size(px(7.)).rounded_full();
                if failed {
                    d.bg(cx.theme().danger)
                } else if running {
                    d.bg(cx.theme().muted_foreground)
                } else {
                    d.border_1()
                        .border_color(cx.theme().muted_foreground.opacity(0.55))
                }
            };
            list = list.child(
                h_flex()
                    .id(gpui::SharedString::from(format!("panel-mark-{row}")))
                    .items_center()
                    .gap(px(8.))
                    .px(px(4.))
                    .py(px(3.))
                    .rounded(px(5.))
                    .cursor_pointer()
                    .hover(|s| s.bg(gpui::rgb(sf.hover)))
                    .on_click(cx.listener(move |_this, _, _window, cx| {
                        leaf.update(cx, |view, cx| {
                            view.scroll_to_mark(row, cx);
                        });
                    }))
                    .child(dot)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            // Commands are code: the mono face sets them apart from
                            // the sans labels and lines the list up like a log.
                            .text_size(px(12.))
                            .font_family(mono.clone())
                            .text_color(if failed {
                                cx.theme().danger
                            } else {
                                cx.theme().foreground
                            })
                            .child(one_line(&mark.text)),
                    )
                    // Only nonzero exits earn a badge. Annotating every success
                    // with a `0` would make the failures harder to spot, not
                    // easier — the whole point of the column.
                    .when_some(mark.exit.filter(|c| *c != 0), |this, code| {
                        this.child(
                            div()
                                .flex_none()
                                .text_size(px(10.5))
                                .font_family(mono.clone())
                                .text_color(cx.theme().danger)
                                .child(code.to_string()),
                        )
                    }),
            );
        }
        self.panel_scroll(list.into_any_element(), title)
    }

    // ── Changes ─────────────────────────────────────────────────────────────

    /// The working-tree diff as a compact file list — path plus `+N −M` — not the
    /// diff overlay's hunk cards, which need far more than 260px to be readable.
    /// Clicking a row opens the full overlay on that repo.
    fn render_panel_changes(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let sf = cx.global::<crate::ui::presets::Surfaces>().sidebar;
        // The pane's own host, and the cwd it resolved its git line through —
        // so the Changes list describes the same repository the sidebar's
        // `+N −M` does, on the same machine.
        let target = self
            .tabs
            .get(self.active)
            .and_then(|t| t.detail_pane(window, cx))
            .and_then(|leaf| {
                let v = leaf.read(cx);
                let cwd = v
                    .git_status_cwd()
                    .map(|p| p.to_path_buf())
                    .or_else(|| v.host_cwd())?;
                Some((v.host(cx)?, cwd))
            });

        let Some((host, cwd)) = target else {
            let title = self.panel_title("Changes", None, None, window, cx);
            return self.panel_scroll(
                self.panel_empty(
                    "No working directory.",
                    Some("This pane has not reported one yet."),
                    cx,
                ),
                title,
            );
        };
        // Probe on first paint for this cwd, and whenever the pane moves to a
        // different repository. Refreshes ride the same git-status observer the
        // sidebar counts do (see `right_panel_refresh_changes`), which re-probes
        // *in place* — the list only blanks when the repository itself changes.
        let key = (host.id(), cwd.clone());
        if self.right_panel.diff_cwd.as_ref() != Some(&key) {
            self.right_panel.diff_cwd = Some(key);
            self.right_panel.diff = None;
            self.spawn_right_panel_diff(host.clone(), cwd.clone(), cx);
        } else if self.right_panel.diff.is_none() && !self.right_panel.diff_loading {
            // Nothing cached and nothing in flight: a probe for a previous cwd
            // landed after we had already moved on and dropped its result, so
            // no one is left to answer for this one. Without this the tab would
            // sit on "Loading…" until some unrelated event nudged it.
            self.spawn_right_panel_diff(host.clone(), cwd.clone(), cx);
        }

        // Count of changed files for the header tally — computed before the title
        // so the diff borrow ends before `panel_title` takes `&mut cx`.
        let count = match &self.right_panel.diff {
            Some(Some(snap)) => {
                let n = snap.files.len() + snap.untracked.len();
                (n > 0).then(|| n.to_string())
            }
            _ => None,
        };
        let title = self.panel_title("Changes", count, None, window, cx);
        let mono = cx.theme().mono_font_family.clone();

        let inner = match &self.right_panel.diff {
            None => self.panel_empty("Loading…", None, cx),
            Some(None) => self.panel_empty(
                "Not a git repository.",
                Some("cd into one and this tab lists its uncommitted changes."),
                cx,
            ),
            Some(Some(snap)) if snap.files.is_empty() && snap.untracked.is_empty() => self
                .panel_empty(
                    "No uncommitted changes.",
                    Some("The working tree is clean."),
                    cx,
                ),
            Some(Some(snap)) => {
                let files: Vec<(String, u32, u32)> = snap
                    .files
                    .iter()
                    .map(|f| (f.path.clone(), f.added, f.removed))
                    .collect();
                let untracked = snap.untracked.clone();
                let focused = self.diff_overlay_focus(host.id(), &cwd).map(str::to_string);
                // Rows inset themselves rather than the list, so the hover and
                // selected capsules bleed a little past the text into the same
                // 12px gutter the tab rail's rows use.
                let mut list = v_flex().px(px(CONTENT_INSET - 4.)).py(px(2.)).gap(px(1.));
                for (path, added, removed) in files {
                    let selected = focused.as_deref() == Some(path.as_str());
                    list = list.child(
                        h_flex()
                            .id(gpui::SharedString::from(format!("panel-change-{path}")))
                            .items_center()
                            .gap(px(8.))
                            .px(px(4.))
                            .py(px(3.))
                            .rounded(px(5.))
                            .cursor_pointer()
                            // The rail's own ladder. Hover used to be this fill at
                            // 55% alpha, which on a light theme is a tint nobody
                            // can see — the same mistake `chrome_tile_variant_for`
                            // already documents having fixed in the title bar, made
                            // again here because there was nothing to reuse.
                            .hover(|s| s.bg(gpui::rgb(sf.hover)))
                            .when(selected, |s| s.bg(gpui::rgb(sf.selected)))
                            .on_click({
                                let host_id = host.id();
                                let cwd = cwd.clone();
                                let path = path.clone();
                                cx.listener(move |this, _, window, cx| {
                                    // Toggling on the same row closes the overlay,
                                    // so a row is a switch for "show me this diff",
                                    // not a one-way door.
                                    this.toggle_diff_overlay_at(
                                        host_id,
                                        cwd.clone(),
                                        Some(path.clone()),
                                        window,
                                        cx,
                                    );
                                })
                            })
                            // A neutral status letter, kind by glyph not by hue —
                            // tracked edits are `M`; untracked get `U` below.
                            .child(git_badge("M", cx.theme().muted_foreground, &mono))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(12.))
                                    .font_family(mono.clone())
                                    .text_color(cx.theme().foreground)
                                    .child(path),
                            )
                            // +N / −M keep the terminal-git greens and reds, the
                            // one place hue earns its keep; a zero side is dropped
                            // rather than shown as `+0`.
                            .when(added > 0, |this| {
                                this.child(
                                    div()
                                        .flex_none()
                                        .text_size(px(11.))
                                        .font_family(mono.clone())
                                        .text_color(cx.theme().success)
                                        .child(format!("+{added}")),
                                )
                            })
                            .when(removed > 0, |this| {
                                this.child(
                                    div()
                                        .flex_none()
                                        .text_size(px(11.))
                                        .font_family(mono.clone())
                                        .text_color(cx.theme().danger)
                                        .child(format!("−{removed}")),
                                )
                            }),
                    );
                }
                if !untracked.is_empty() {
                    list = list.child(
                        h_flex()
                            .items_center()
                            .gap(px(8.))
                            .px(px(4.))
                            .py(px(3.))
                            .child(git_badge(
                                "U",
                                cx.theme().muted_foreground.opacity(0.75),
                                &mono,
                            ))
                            .child(
                                div()
                                    .text_size(px(11.5))
                                    .text_color(cx.theme().muted_foreground)
                                    .child(format!("{} untracked", untracked.len())),
                            ),
                    );
                }
                list.into_any_element()
            }
        };
        self.panel_scroll(inner, title)
    }

    /// Off-thread `git diff` for the panel, mirroring the diff overlay's probe.
    fn spawn_right_panel_diff(
        &mut self,
        host: crate::ui::host_ops::SharedHost,
        cwd: PathBuf,
        cx: &mut Context<Self>,
    ) {
        if self.right_panel.diff_loading {
            return;
        }
        self.right_panel.diff_loading = true;
        let key = (host.id(), cwd.clone());
        crate::ui::host_ops::HostOps::run(
            host,
            cx,
            move |h| git_diff::probe(h, &cwd),
            move |app, result, cx| {
                app.right_panel.diff_loading = false;
                // Drop the result if the panel moved on to another repo — or
                // another machine — while we flew; otherwise a slow probe would
                // overwrite a newer one.
                if app.right_panel.diff_cwd.as_ref() == Some(&key) {
                    app.right_panel.diff = Some(result);
                    cx.notify();
                }
            },
        );
    }

    /// Re-probe the Changes list when the shared status cache learned something
    /// newer than what's shown — called from the app's
    /// `observe_global::<GitStatusCache>` hook, the same trigger that refreshes
    /// the sidebar's `+N −M` and the diff overlay.
    ///
    /// Deliberately *not* "drop the cache and let the next paint re-probe":
    /// that observer fires on every landed probe, including unrelated repos', so
    /// dropping the cache blanked the list to "Loading…" and spawned a fresh
    /// `git diff` several times a second while a pane was producing output.
    /// Comparing branch + totals first keeps the quiet case free, and re-probing
    /// in place leaves the rows on screen until the new snapshot lands.
    pub(crate) fn right_panel_refresh_changes(&mut self, cx: &mut Context<Self>) {
        if self.right_panel.diff_loading {
            return;
        }
        let Some((id, cwd)) = self.right_panel.diff_cwd.clone() else {
            return; // never probed — the render path owns the first one
        };
        // The host object itself has to come from the registry: only the id is
        // cached, and a machine that has gone away has no diff to re-probe.
        let Some(host) = crate::ui::host_registry::HostRegistry::get(cx, id) else {
            return;
        };
        // `Some(None)` (probed, not a work tree) stays put: a status entry for a
        // non-repo can't appear, so there's nothing to disagree with.
        let Some(Some(snap)) = &self.right_panel.diff else {
            return;
        };
        let Some(status) = cx
            .try_global::<crate::terminal::git_status::GitStatusCache>()
            .and_then(|cache| cache.status_for(id, &cwd))
        else {
            return;
        };
        let stale = status.branch != snap.branch || (status.added, status.removed) != snap.totals();
        if stale {
            self.spawn_right_panel_diff(host, cwd, cx);
        }
    }

    // ── Files ───────────────────────────────────────────────────────────────

    /// The project tree, reusing the code panel's rows verbatim — same expand
    /// state, same click-to-open, so the panel and the editor overlay are two
    /// views of one tree rather than two trees.
    /// The Files tab follows the pane: a local pane gets its repository tree, a
    /// connected native-SSH pane gets that machine's filesystem over SFTP. One tab,
    /// because "the files this pane is working in" is one idea — where they
    /// physically live is a property of the pane, not a second feature.
    fn render_panel_files(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let remote = self.remote_files_pane(window, cx);
        let host = remote.as_ref().map(|(_, host)| host.clone());
        // Point the browser at this pane, or tear it down when the tab has moved
        // back to a local one. Returns whether to render the remote mode.
        if self.sftp_sync_pane(remote.map(|(id, _)| id), window, cx) {
            return self.render_panel_sftp(host.unwrap_or_default(), window, cx);
        }

        // No header control: the tree's one view option (dotfiles) is a
        // right-click away in the tree itself (`file_tree::dotfiles_menu_item`),
        // which is where you are when you want it.
        let title = self.panel_title("Files", None, None, window, cx);
        let search = self.panel_search(&self.file_search.clone(), cx);
        let rows = self.render_file_tree_rows(window, cx);
        v_flex()
            .flex_1()
            .min_h_0()
            .child(title)
            .child(search)
            .child(rows)
            .into_any_element()
    }

    /// The detail pane and its host name when it's a *connected native* SSH pane —
    /// the gate for the Files tab's remote mode. A foreground `ssh` typed into a
    /// local shell has no connection to browse, and a still-connecting one has
    /// nothing to list, so both keep the local tree.
    fn remote_files_pane(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<(u64, String)> {
        use crate::daemon::protocol::{RemoteKind, SshPhase};
        let leaf = self.tabs.get(self.active)?.detail_pane(window, cx)?;
        let view = leaf.read(cx);
        let remote = view.remote_context()?;
        if remote.kind != RemoteKind::NativeSsh
            || !matches!(view.ssh_phase(), Some(SshPhase::Connected))
        {
            return None;
        }
        Some((view.pane_id, remote.target))
    }
}

/// A small status letter (`M`/`U`/…) for a change row. The *kind* is told by the
/// glyph in the mono face, not by colour, so the list stays monochrome; callers
/// pass a muted tone and reserve real hue for the `+N −M` counts beside it.
pub(crate) fn git_badge(letter: &str, color: gpui::Hsla, mono: &gpui::SharedString) -> AnyElement {
    div()
        .flex_none()
        .w(px(14.))
        .text_center()
        .text_size(px(10.5))
        .font_family(mono.clone())
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(color)
        .child(letter.to_string())
        .into_any_element()
}

/// A pid / port pill: a mono number on the soft-grey capsule the rest of the
/// chrome uses, so a numeric datum reads as a tag rather than loose text.
pub(crate) fn info_chip(
    text: &str,
    bg: gpui::Hsla,
    fg: gpui::Hsla,
    mono: &gpui::SharedString,
) -> AnyElement {
    div()
        .flex_none()
        .px(px(5.))
        .py(px(1.5))
        .rounded(px(4.))
        .bg(bg)
        .text_size(px(10.5))
        .font_family(mono.clone())
        .text_color(fg)
        .child(text.to_string())
        .into_any_element()
}

/// The label for revealing a path in the OS file manager: only macOS has a
/// "Finder", so everywhere else it's the generic "Open Folder". Shared by the
/// Info row, the file-tree context menu and the SFTP job list so the action
/// carries one name per platform.
pub fn reveal_label() -> &'static str {
    if cfg!(target_os = "macos") {
        "Reveal in Finder"
    } else {
        "Open Folder"
    }
}

/// The one-word status the Info row shows next to the agent's name.
fn agent_status_label(status: crate::core::cli_agent::AgentStatus) -> &'static str {
    use crate::core::cli_agent::AgentStatus::*;
    match status {
        Idle => "idle",
        Working => "working",
        Waiting => "waiting",
        Done => "done",
    }
}

/// Flatten a possibly-multiline command to one row: newlines and tabs become
/// spaces, runs of whitespace collapse. A heredoc or a `for` loop typed across
/// lines is still recognizable, and the list keeps one row per command.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `~`-shorten a path for the Info list, which has ~180px to play with.
fn compact_path(path: &std::path::Path) -> String {
    let s = path.to_string_lossy().to_string();
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && s.starts_with(&home) => s.replacen(&home, "~", 1),
        _ => s,
    }
}

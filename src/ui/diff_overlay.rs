//! The working-tree diff overlay: a read-only, GitHub-style side-by-side diff
//! that covers the terminal area when the user clicks a sidebar row's git line
//! (`⎇ branch +N −N`). A scrolling column of per-file cards with collapsible
//! hunk bodies — old on the left, new on the right — plus an untracked-files
//! section `git diff` itself can't show.
//!
//! Deliberately a *lens*, not a git client: no staging, no discard. The
//! terminal keeps running underneath (the overlay covers
//! only the body area, never the sidebar, so other tabs' git lines stay
//! clickable to switch which repo is shown). The overlay belongs to the tab it
//! was opened on: switching tabs hides it, switching back restores it, closing
//! the tab drops it. Esc, the ✕, or re-clicking the same git line closes it.
//!
//! Data comes from [`crate::terminal::git_diff`], probed off-thread on open
//! and re-probed automatically while open whenever the shared
//! [`GitStatusCache`](crate::terminal::git_status::GitStatusCache) lands a
//! snapshot whose branch or counts disagree with what's shown — so a finishing
//! command or agent turn refreshes the overlay through the exact trigger
//! machinery the sidebar numbers already use.

use std::collections::HashSet;
use std::path::PathBuf;

use gpui::{
    AnyElement, FocusHandle, FontWeight, KeyDownEvent, Pixels, Window, div, prelude::*, px,
};
use gpui_component::button::Button;
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};

use crate::terminal::git_diff::{
    self, AUTO_COLLAPSE_LINES, DiffSnapshot, FileDiff, FileStatus, LineKind,
};
use crate::ui::app::Tty7App;
use crate::ui::rounding;
use crate::ui::rounding::RoundedCorners as _;

/// What the overlay currently shows: probing, a parsed snapshot, or the
/// answer that the cwd stopped being a repo.
pub(crate) enum DiffLoad {
    /// First probe still in flight.
    Loading,
    Ready(DiffSnapshot),
    /// The probe came back "not a work tree" (repo deleted, dir gone).
    NotARepo,
}

/// State of an open diff overlay (`None` on its [`Tab`](crate::ui::app::Tab)
/// when closed). Per-tab: switching tabs hides/restores it, closing the tab
/// drops it; only the active tab's overlay is rendered.
pub(crate) struct DiffOverlayState {
    /// The machine the diff is read from — the pane's own host, so an overlay
    /// opened on a pane whose repository lives elsewhere shows *that*
    /// repository. Part of the toggle key together with `cwd`: the same path on
    /// two machines is two different diffs.
    ///
    /// The id, not the host object: an overlay outlives a reconnect (it is only
    /// dropped by closing it or its tab), and the object it was opened with
    /// belongs to the connection that has since been replaced. Every re-probe
    /// resolves the id afresh, so a reconnected machine's next refresh lands
    /// instead of failing forever against a dead client.
    pub(crate) host_id: crate::ui::host_ops::HostId,
    /// The pane cwd the diff is probed from — the same path the clicked git
    /// line resolved its status through, so overlay and sidebar agree on the
    /// repo. Also the toggle key: re-clicking a line with this cwd closes.
    pub(crate) cwd: PathBuf,
    /// Focus target so Esc lands on the overlay's key handler.
    pub(crate) focus_handle: FocusHandle,
    pub(crate) load: DiffLoad,
    /// A probe is currently in flight (initial or refresh).
    pub(crate) loading: bool,
    /// Files the user flipped away from their default collapse state (small
    /// files default open, big/binary ones closed). Keyed by path so the set
    /// survives a background refresh of the snapshot.
    pub(crate) toggled: HashSet<String>,
    /// When set, the overlay shows only this file (repo-relative path), always
    /// expanded — the "click a row in the Changes panel" entry point. `None` is
    /// the whole-tree view the git line opens. Kept as a path rather than an
    /// index so a background re-probe that reorders files doesn't swap which
    /// file is on screen; a path that vanishes from the diff falls back to the
    /// full list rather than showing an empty overlay.
    pub(crate) focus: Option<String>,
}

impl Tty7App {
    /// Open the diff overlay for `cwd` — or close it when it's already open
    /// for that same cwd (the git line acts as a toggle). Opening for a
    /// different cwd swaps the overlay's repo in place.
    pub(crate) fn toggle_diff_overlay(
        &mut self,
        host: crate::ui::host_ops::HostId,
        cwd: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_diff_overlay_at(host, cwd, None, window, cx)
    }

    /// The same toggle, scoped to one file: opens the overlay showing only
    /// `focus` (repo-relative), which is what the Changes panel's rows do. The
    /// toggle key is the pair — re-clicking the row that's already on screen
    /// closes, while clicking a *different* row swaps the shown file in place
    /// without the overlay blinking shut and re-probing.
    pub(crate) fn toggle_diff_overlay_at(
        &mut self,
        host: crate::ui::host_ops::HostId,
        cwd: PathBuf,
        focus: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let active = self.active;
        // Was the diff already the front overlay? If it was buried under the
        // code panel, this click means "bring it up", not "close it" — closing
        // something the user can't currently see would read as the click doing
        // nothing.
        let was_front = self.tabs.get(active).is_some_and(|t| {
            t.overlay_top == crate::ui::app::OverlayTop::Diff || !self.code_panel_visible()
        });
        // Acting on the diff raises it over the code panel, whether it was
        // already open or not.
        if let Some(tab) = self.tabs.get_mut(active) {
            tab.overlay_top = crate::ui::app::OverlayTop::Diff;
        }
        match self
            .tabs
            .get_mut(active)
            .and_then(|t| t.diff_overlay.as_mut())
            .filter(|o| o.cwd == cwd && o.host_id == host)
        {
            // Already open on this repo showing this exact thing, and already on
            // top — toggle off.
            Some(o) if o.focus == focus && was_front => {
                self.close_diff_overlay(window, cx);
                return;
            }
            // Open on this repo, different file: retarget. The snapshot is
            // already loaded and covers every file, so there is nothing to
            // re-probe — this is a pure re-render.
            Some(o) => {
                o.focus = focus;
                // Take focus too, so Esc closes the diff rather than whatever
                // was focused before it came forward (often the editor).
                let handle = o.focus_handle.clone();
                window.focus(&handle, cx);
                cx.notify();
                return;
            }
            None => {}
        }
        // The overlay steals focus (it needs Esc); snapshot the active pane so
        // closing lands back on the same terminal — same discipline as Settings.
        self.remember_active_pane(window, cx);
        let Some(tab) = self.tabs.get_mut(active) else {
            return; // home page — no tab body to overlay
        };
        let focus_handle = cx.focus_handle();
        tab.diff_overlay = Some(DiffOverlayState {
            host_id: host,
            cwd,
            focus_handle: focus_handle.clone(),
            load: DiffLoad::Loading,
            loading: false,
            toggled: HashSet::new(),
            focus,
        });
        window.focus(&focus_handle, cx);
        self.spawn_diff_probe(cx);
        cx.notify();
    }

    /// The file the active tab's overlay is currently scoped to, if any — the
    /// Changes panel reads it to mark the matching row as selected, so panel and
    /// overlay can't disagree about what's on screen.
    pub(crate) fn diff_overlay_focus(
        &self,
        host: crate::ui::host_ops::HostId,
        cwd: &std::path::Path,
    ) -> Option<&str> {
        let overlay = self.tabs.get(self.active)?.diff_overlay.as_ref()?;
        (overlay.cwd == cwd && overlay.host_id == host).then_some(overlay.focus.as_deref())?
    }

    /// Close the active tab's overlay (Esc, ✕, or the toggle) and give focus
    /// back to the active terminal.
    pub(crate) fn close_diff_overlay(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let active = self.active;
        let taken = self
            .tabs
            .get_mut(active)
            .and_then(|t| t.diff_overlay.take());
        if taken.is_some() {
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    /// Kick off an off-thread full-diff probe for the overlay's cwd. In-flight
    /// dedup is a simple flag: refresh triggers while one flies are dropped —
    /// the status cache will fire again on the next real change, and a
    /// just-landed diff is fresh enough.
    fn spawn_diff_probe(&mut self, cx: &mut Context<Self>) {
        let active = self.active;
        let Some(overlay) = self
            .tabs
            .get_mut(active)
            .and_then(|t| t.diff_overlay.as_mut())
        else {
            return;
        };
        if overlay.loading {
            return;
        }
        let cwd = overlay.cwd.clone();
        let id = overlay.host_id;
        // A machine that is not registered has nothing to probe. Leave `loading`
        // alone so the overlay keeps the snapshot it has (or its loading state)
        // and the next trigger tries again — a reconnect re-registers the id.
        let Some(host) = crate::ui::host_registry::HostRegistry::lookup(cx, id) else {
            return;
        };
        overlay.loading = true;
        let probe_cwd = cwd.clone();
        crate::ui::host_ops::HostOps::run(
            host,
            cx,
            move |h| git_diff::probe(h, &probe_cwd),
            move |app, result, cx| {
                // Land on every tab whose overlay shows this repo on this
                // machine — the spawning tab may no longer be active, and
                // sibling tabs on the same repo are equally stale. A slot
                // closed or swapped to another repo while we flew is skipped.
                let mut landed = false;
                for tab in app.tabs.iter_mut() {
                    let Some(overlay) = tab
                        .diff_overlay
                        .as_mut()
                        .filter(|o| o.cwd == cwd && o.host_id == id)
                    else {
                        continue;
                    };
                    overlay.loading = false;
                    overlay.load = match &result {
                        Some(snap) => DiffLoad::Ready(snap.clone()),
                        None => DiffLoad::NotARepo,
                    };
                    landed = true;
                }
                if landed {
                    cx.notify();
                }
            },
        );
    }

    /// Re-probe the open overlay when the shared status cache learned
    /// something newer than what's shown — called from the app's
    /// `observe_global::<GitStatusCache>` hook, i.e. on the very triggers
    /// (command end, agent-turn end, cwd change) that refresh the sidebar
    /// numbers. Comparing branch + totals keeps the quiet case (unrelated
    /// repo's probe landing) from spawning needless `git diff` runs.
    pub(crate) fn maybe_refresh_diff_overlay(&mut self, cx: &mut Context<Self>) {
        // Only the active tab's overlay is visible; hidden ones catch up via
        // this same check when their tab is activated (`activate` calls us).
        let Some(overlay) = self
            .tabs
            .get(self.active)
            .and_then(|t| t.diff_overlay.as_ref())
        else {
            return;
        };
        if overlay.loading {
            return;
        }
        let DiffLoad::Ready(snap) = &overlay.load else {
            return; // initial probe pending, or repo gone — nothing to diff against
        };
        let Some(status) = cx
            .try_global::<crate::terminal::git_status::GitStatusCache>()
            .and_then(|cache| cache.status_for(overlay.host_id, &overlay.cwd))
        else {
            return;
        };
        if status.branch != snap.branch || (status.added, status.removed) != snap.totals() {
            self.spawn_diff_probe(cx);
        }
    }

    /// The overlay element, or `None` when closed. Mounted as the topmost
    /// absolute child of the body area — it covers the terminal but not the
    /// sidebar or title strip.
    pub(crate) fn render_diff_overlay(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let overlay = self.tabs.get(self.active)?.diff_overlay.as_ref()?;

        let content = match &overlay.load {
            DiffLoad::Loading => self.diff_message("Reading diff…", cx),
            DiffLoad::NotARepo => self.diff_message("Not a git repository", cx),
            DiffLoad::Ready(snap) if snap.files.is_empty() && snap.untracked.is_empty() => {
                self.diff_message("Working tree clean", cx)
            }
            DiffLoad::Ready(snap) => {
                self.diff_file_list(snap, &overlay.toggled, focused_file(snap, overlay), cx)
            }
        };

        let header = self.diff_header(overlay, cx);

        Some(
            v_flex()
                .absolute()
                .inset_0()
                // Blocks mouse from reaching the terminal underneath.
                .occlude()
                // Same gradient/opacity-aware paint as the root and the settings
                // overlay, so a gradient or image theme doesn't snap to a flat
                // color here. On a translucent theme this second layer compounds
                // the alpha a little — deliberate: the overlay must occlude the
                // terminal behind it to stay readable.
                .bg(
                    match cx.try_global::<crate::ui::presets::ActiveBackground>() {
                        Some(bg) => crate::ui::theme::window_background(bg),
                        None => cx.theme().background.into(),
                    },
                )
                .text_color(cx.theme().foreground)
                .track_focus(&overlay.focus_handle)
                .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                    if ev.keystroke.key.as_str() == "escape" {
                        this.close_diff_overlay(window, cx);
                    }
                }))
                .child(header)
                .child(content)
                .into_any_element(),
        )
    }

    /// Top bar: branch, file/line totals, a subtle refresh spinner slot, ✕.
    fn diff_header(
        &self,
        overlay: &DiffOverlayState,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let (branch, files, untracked, added, removed) = match &overlay.load {
            DiffLoad::Ready(s) => {
                let (a, r) = s.totals();
                (s.branch.clone(), s.files.len(), s.untracked.len(), a, r)
            }
            _ => (String::new(), 0, 0, 0, 0),
        };
        // The overlay now covers the title strip, so its header *is* the title
        // bar for as long as it's up: same height, and the same left inset the
        // editor header uses — content clears the traffic lights whenever the
        // rail isn't there to hold that space for us.
        let lead = if self.left_panel_open(cx) {
            crate::ui::app::CONTENT_INSET
        } else {
            crate::ui::app::TITLE_BAR_LEAD
        };
        // Standing in for the title bar means carrying its gestures too: the
        // overlay covers the real bar, so without this the whole top of the window
        // stops moving it while a diff is up.
        crate::ui::app::title_bar_drag(h_flex().id("diff-overlay-header"))
            .flex_shrink_0()
            .h(px(crate::ui::app::TITLE_BAR_HEIGHT))
            .pl(px(lead))
            // Trailing tile aligns on its glyph's ink, like every corner control.
            .pr(px(crate::ui::app::tile_trailing_inset()))
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                gpui::svg()
                    .path("icons/git-branch.svg")
                    .flex_shrink_0()
                    .size(px(13.))
                    .text_color(cx.theme().muted_foreground),
            )
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .child(branch),
            )
            // Scoped to one file: the branch stays (it's still what we diff
            // against) but the totals give way to the file's own name, with a
            // click target back to the whole tree — otherwise the only way out
            // of a focused view would be to close and re-open the overlay.
            .when_some(focused_name(overlay), |bar, name| {
                // Wrapped like every other control on a drag row — see the header's
                // own note: HTCAPTION would otherwise swallow the click on Windows.
                bar.child(
                    div().occlude().flex_shrink_0().child(
                        h_flex()
                            .id("diff-overlay-unfocus")
                            .items_center()
                            .gap_1()
                            .px_1p5()
                            .py_0p5()
                            .rounded_md()
                            .cursor_pointer()
                            .hover(|s| s.bg(cx.theme().list_hover))
                            .on_click(cx.listener(|this, _, _window, cx| {
                                let active = this.active;
                                if let Some(overlay) = this
                                    .tabs
                                    .get_mut(active)
                                    .and_then(|t| t.diff_overlay.as_mut())
                                {
                                    overlay.focus = None;
                                    cx.notify();
                                }
                            }))
                            .child(
                                Icon::new(IconName::ChevronLeft)
                                    .small()
                                    .text_color(cx.theme().muted_foreground),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .font_family(self.font_family.clone())
                                    .child(name),
                            ),
                    ),
                )
            })
            .when(
                matches!(overlay.load, DiffLoad::Ready(_)) && overlay.focus.is_none(),
                |bar| {
                    let mut summary = format!(
                        "{} changed file{}",
                        files,
                        if files == 1 { "" } else { "s" }
                    );
                    if untracked > 0 {
                        summary.push_str(&format!(" · {untracked} untracked"));
                    }
                    bar.child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(summary),
                    )
                    .when(added > 0, |bar| {
                        bar.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().success)
                                .child(format!("+{added}")),
                        )
                    })
                    .when(removed > 0, |bar| {
                        bar.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().danger)
                                .child(format!("−{removed}")),
                        )
                    })
                },
            )
            // A quiet "refreshing" hint while a re-probe flies over stale data.
            .when(
                overlay.loading && matches!(overlay.load, DiffLoad::Ready(_)),
                |bar| {
                    bar.child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("refreshing…"),
                    )
                },
            )
            .child(div().flex_1())
            .child(
                div().occlude().flex_shrink_0().child(
                    crate::ui::tab_strip::chrome_tile_sized(
                        // Explicit tile, not `.small()`: this bar stands in for the
                        // title bar while the overlay is up, so its close control is
                        // the same tile the title bar's controls are.
                        Button::new("diff-overlay-close").icon(Icon::new(IconName::Close)),
                        crate::ui::app::TILE_SIZE,
                        crate::ui::app::TILE_GLYPH_LINE,
                        false,
                        cx,
                    )
                    .rounded_lg()
                    .tooltip("Close Diff (Esc)")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.close_diff_overlay(window, cx);
                    })),
                ),
            )
    }

    /// A centered single-line state (loading / clean / not-a-repo).
    fn diff_message(&self, text: &'static str, cx: &Context<Self>) -> AnyElement {
        div()
            .flex_1()
            .flex()
            .items_center()
            .justify_center()
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .child(text)
            .into_any_element()
    }

    /// The scrolling column of per-file diff cards plus the untracked section.
    fn diff_file_list(
        &self,
        snap: &DiffSnapshot,
        toggled: &HashSet<String>,
        focused: Option<usize>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut list = v_flex().gap_3().p_4().w_full();
        for (idx, file) in snap.files.iter().enumerate() {
            if focused.is_some_and(|f| f != idx) {
                continue;
            }
            // A file opened by name was asked for explicitly — show its body
            // even when it's over the auto-collapse threshold. The header still
            // toggles, so a huge file can be folded back down.
            let expanded = if focused == Some(idx) {
                !toggled.contains(&file.path)
            } else {
                file_expanded(file, toggled)
            };
            list = list.child(self.diff_file_card(idx, file, expanded, cx));
        }
        // Untracked files are a property of the tree, not of the focused file.
        if focused.is_none() && !snap.untracked.is_empty() {
            list = list.child(self.diff_untracked_section(&snap.untracked, cx));
        }
        div()
            .id("diff-overlay-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .child(list)
            .into_any_element()
    }

    /// One file's card: a clickable header row and, when expanded, the hunks.
    fn diff_file_card(
        &self,
        idx: usize,
        file: &FileDiff,
        expanded: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Binary files and pure renames have no hunk body to reveal; their
        // header is inert (no chevron, no click).
        let expandable = !file.binary && !file.hunks.is_empty();
        let (glyph, glyph_color) = match file.status {
            FileStatus::Added => ("A", cx.theme().success),
            FileStatus::Modified => ("M", cx.theme().warning),
            FileStatus::Deleted => ("D", cx.theme().danger),
            FileStatus::Renamed => ("R", cx.theme().muted_foreground),
        };
        // `old → new` for renames, the plain path otherwise.
        let shown_path = match &file.old_path {
            Some(old) => format!("{old} → {}", file.path),
            None => file.path.clone(),
        };

        // Whether the body paints anything at all. `expanded` alone is not that
        // question: a binary file or a pure rename has no hunks and is not
        // truncated, so its body is empty and the header *is* the card. A
        // truncated file with no parsable hunks still renders the notice.
        let has_body = expanded && (!file.hunks.is_empty() || file.truncated);

        // The header paints a solid band flush into the card's corners, and the
        // card's `overflow_hidden` cannot round it — that clip is a square,
        // unantialiased scissor (issue #236, see `ui::rounding`). So the band
        // carries the radius: top two when a body follows it, all four when the
        // header is the card's only band.
        let header_corners = rounding::stack_corners(
            0,
            if has_body { 2 } else { 1 },
            rounding::CARD_RADIUS,
            rounding::HAIRLINE,
        );
        let mut header = h_flex()
            .id(("diff-file-header", idx))
            .w_full()
            .items_center()
            .gap_2()
            .px_2p5()
            .py_1p5()
            .rounded_corners(header_corners)
            .bg(cx.theme().secondary)
            .when(expandable, |h| {
                let path = file.path.clone();
                h.cursor_pointer()
                    .hover(|s| s.bg(cx.theme().list_hover))
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        let active = this.active;
                        if let Some(overlay) = this
                            .tabs
                            .get_mut(active)
                            .and_then(|t| t.diff_overlay.as_mut())
                        {
                            // Flip this file's override; removing an existing
                            // entry returns it to its default state.
                            if !overlay.toggled.remove(&path) {
                                overlay.toggled.insert(path.clone());
                            }
                            cx.notify();
                        }
                    }))
                    .child(
                        Icon::new(if expanded {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .small()
                        .text_color(cx.theme().muted_foreground),
                    )
            })
            .child(
                div()
                    .flex_shrink_0()
                    .font_family(self.font_family.clone())
                    .text_xs()
                    .font_weight(FontWeight::BOLD)
                    .text_color(glyph_color)
                    .child(glyph),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_xs()
                    .font_family(self.font_family.clone())
                    .child(shown_path),
            );
        if file.binary {
            header = header.child(
                div()
                    .flex_shrink_0()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("binary"),
            );
        }
        if file.added > 0 {
            header = header.child(
                div()
                    .flex_shrink_0()
                    .text_xs()
                    .text_color(cx.theme().success)
                    .child(format!("+{}", file.added)),
            );
        }
        if file.removed > 0 {
            header = header.child(
                div()
                    .flex_shrink_0()
                    .text_xs()
                    .text_color(cx.theme().danger)
                    .child(format!("−{}", file.removed)),
            );
        }

        let mut card = v_flex()
            .w_full()
            .border_1()
            .border_color(cx.theme().border)
            .rounded(rounding::CARD_RADIUS)
            // Overflow backstop only; the bands inside round themselves.
            .overflow_hidden()
            .child(header);

        if has_body {
            let mut body = v_flex().w_full();
            // Split every hunk up front so the *last* row is knowable: a diff
            // cell paints a tint, and the card's clip is square, so the row that
            // ends the card has to draw the bottom corners itself. A truncation
            // notice (no fill of its own) takes that job away again.
            let hunks: Vec<_> = file
                .hunks
                .iter()
                .map(|hunk| (hunk, split_hunk(&hunk.lines)))
                .collect();
            let closing_row = if file.truncated {
                None
            } else {
                hunks
                    .iter()
                    .rposition(|(_, rows)| !rows.is_empty())
                    .map(|h| (h, hunks[h].1.len() - 1))
            };
            for (h, (hunk, rows)) in hunks.iter().enumerate() {
                body = body.child(
                    div()
                        .w_full()
                        .px_2()
                        .py_0p5()
                        .bg(cx.theme().muted)
                        .text_xs()
                        .font_family(self.font_family.clone())
                        .text_color(cx.theme().muted_foreground)
                        .truncate()
                        .child(hunk.header.clone()),
                );
                for (r, row) in rows.iter().enumerate() {
                    body = body.child(self.diff_split_row(row, closing_row == Some((h, r)), cx));
                }
            }
            if file.truncated {
                body = body.child(
                    div()
                        .w_full()
                        .px_2()
                        .py_1()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!(
                            "Diff truncated at {} lines — run `git diff` in the terminal for the rest.",
                            git_diff::MAX_LINES_PER_FILE
                        )),
                );
            }
            card = card.child(body);
        }
        card.into_any_element()
    }

    /// One side-by-side row: the old (left) and new (right) cells with a hairline
    /// splitter between them. A `None` cell — no counterpart on that side —
    /// paints a muted placeholder so a pure add/remove reads as one column empty.
    ///
    /// `closes_card` marks the row that sits on the card's bottom edge; its two
    /// outer cells then round their outer bottom corner, since the card's clip
    /// cannot do it for them (see `ui::rounding`).
    fn diff_split_row(&self, row: &SplitRow, closes_card: bool, cx: &Context<Self>) -> AnyElement {
        let radius = if closes_card {
            rounding::inner_radius(rounding::CARD_RADIUS, rounding::HAIRLINE)
        } else {
            px(0.)
        };
        h_flex()
            .w_full()
            // Fixed row height so blank diff lines don't collapse.
            .h(px(19.))
            .items_stretch()
            .text_xs()
            .font_family(self.font_family.clone())
            .child(self.diff_split_cell(row.left.as_ref(), Side::Old, radius, cx))
            .child(div().flex_shrink_0().w(px(1.)).bg(cx.theme().border))
            .child(self.diff_split_cell(row.right.as_ref(), Side::New, radius, cx))
            .into_any_element()
    }

    /// One half of a split row: a right-aligned line-number gutter, then the
    /// marker and text in the terminal font, tinted green/red when changed.
    ///
    /// `outer_radius` rounds the cell's own outer bottom corner — non-zero only
    /// on the row that closes the card, whose tint would otherwise square that
    /// corner off (see `ui::rounding`).
    fn diff_split_cell(
        &self,
        cell: Option<&SplitCell>,
        side: Side,
        outer_radius: Pixels,
        cx: &Context<Self>,
    ) -> AnyElement {
        let base = h_flex().flex_1().min_w_0().h_full().items_center();
        let base = match side {
            Side::Old => base.rounded_bl(outer_radius),
            Side::New => base.rounded_br(outer_radius),
        };
        let Some(cell) = cell else {
            return base.bg(cx.theme().muted.opacity(0.3)).into_any_element();
        };
        let (marker, tint) = match (cell.changed, side) {
            (true, Side::Old) => ("−", Some(cx.theme().danger.opacity(0.12))),
            (true, Side::New) => ("+", Some(cx.theme().success.opacity(0.12))),
            (false, _) => (" ", None),
        };
        base.when_some(tint, |row, bg| row.bg(bg))
            .child(
                h_flex()
                    .flex_shrink_0()
                    .w(px(42.))
                    .justify_end()
                    .pr_1p5()
                    .text_color(cx.theme().muted_foreground.opacity(0.7))
                    .child(cell.no.map(|n| n.to_string()).unwrap_or_default()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(format!("{marker} {}", cell.text)),
            )
            .into_any_element()
    }

    /// The trailing "Untracked files" section: names only — `git diff HEAD`
    /// has no blob to diff a never-added file against, but hiding them would
    /// read as lost work (agents create files constantly).
    fn diff_untracked_section(&self, untracked: &[String], cx: &Context<Self>) -> AnyElement {
        // Same filled-band-in-a-rounded-card shape as `diff_file_card`, so the
        // header owns the corners it sits in. The rows below it paint no fill,
        // which is why only the top pair is ever non-zero here.
        let header_corners = rounding::stack_corners(
            0,
            if untracked.is_empty() { 1 } else { 2 },
            rounding::CARD_RADIUS,
            rounding::HAIRLINE,
        );
        let mut section = v_flex()
            .w_full()
            .border_1()
            .border_color(cx.theme().border)
            .rounded(rounding::CARD_RADIUS)
            .overflow_hidden()
            .child(
                div()
                    .w_full()
                    .px_2p5()
                    .py_1p5()
                    .rounded_corners(header_corners)
                    .bg(cx.theme().secondary)
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!("Untracked files ({})", untracked.len())),
            );
        for path in untracked {
            section = section.child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap_2()
                    .px_2p5()
                    .py_1()
                    .text_xs()
                    .font_family(self.font_family.clone())
                    .child(
                        div()
                            .flex_shrink_0()
                            .font_weight(FontWeight::BOLD)
                            .text_color(cx.theme().success)
                            .child("A"),
                    )
                    .child(div().flex_1().min_w_0().truncate().child(path.clone())),
            );
        }
        section.into_any_element()
    }
}

/// Resolve the overlay's focused path to an index into `snap.files`. `None`
/// means "show everything" — either nothing is focused, or the focused path is
/// no longer in the diff (the user reverted it while the overlay was open), in
/// which case falling back to the full list beats an empty screen.
fn focused_file(snap: &DiffSnapshot, overlay: &DiffOverlayState) -> Option<usize> {
    let path = overlay.focus.as_deref()?;
    snap.files.iter().position(|f| f.path == path)
}

/// The focused file's name for the header, only once it's known to be in the
/// snapshot — so a stale focus doesn't label a list that shows every file.
fn focused_name(overlay: &DiffOverlayState) -> Option<String> {
    let DiffLoad::Ready(snap) = &overlay.load else {
        return None;
    };
    let idx = focused_file(snap, overlay)?;
    Some(snap.files[idx].path.clone())
}

/// Whether a file's body shows: small text diffs default open, big ones (and
/// anything the user explicitly flipped) invert via the `toggled` set.
fn file_expanded(file: &FileDiff, toggled: &HashSet<String>) -> bool {
    let default_open = file.added + file.removed <= AUTO_COLLAPSE_LINES;
    default_open != toggled.contains(&file.path)
}

/// Which column a split cell belongs to — picks the marker and tint.
#[derive(Clone, Copy)]
enum Side {
    Old,
    New,
}

/// One half of a side-by-side row. `changed` distinguishes an added/removed
/// line (tinted) from a context line (plain, shown identically on both sides).
struct SplitCell {
    no: Option<u32>,
    text: String,
    changed: bool,
}

/// A side-by-side row: old on the left, new on the right. Either side is `None`
/// when a change block is longer on the other side (pure add/remove, or an
/// uneven replacement).
struct SplitRow {
    left: Option<SplitCell>,
    right: Option<SplitCell>,
}

/// Pair a hunk's unified lines into side-by-side rows: removed lines fill the
/// left column, added lines the right, and a context line flushes any pending
/// change block before landing on both sides. Within a block the two columns
/// align positionally (i-th removed ↔ i-th added), leftovers pair with `None`.
fn split_hunk(lines: &[git_diff::DiffLine]) -> Vec<SplitRow> {
    // Tabs don't expand in UI text layout; four spaces keeps indentation readable.
    fn clean(text: &str) -> String {
        text.replace('\t', "    ")
    }
    fn flush(
        rows: &mut Vec<SplitRow>,
        rem: &mut Vec<&git_diff::DiffLine>,
        add: &mut Vec<&git_diff::DiffLine>,
    ) {
        for i in 0..rem.len().max(add.len()) {
            rows.push(SplitRow {
                left: rem.get(i).map(|l| SplitCell {
                    no: l.old_no,
                    text: clean(&l.text),
                    changed: true,
                }),
                right: add.get(i).map(|l| SplitCell {
                    no: l.new_no,
                    text: clean(&l.text),
                    changed: true,
                }),
            });
        }
        rem.clear();
        add.clear();
    }

    let mut rows = Vec::new();
    let mut rem: Vec<&git_diff::DiffLine> = Vec::new();
    let mut add: Vec<&git_diff::DiffLine> = Vec::new();
    for line in lines {
        match line.kind {
            LineKind::Removed => rem.push(line),
            LineKind::Added => add.push(line),
            LineKind::Context => {
                flush(&mut rows, &mut rem, &mut add);
                rows.push(SplitRow {
                    left: Some(SplitCell {
                        no: line.old_no,
                        text: clean(&line.text),
                        changed: false,
                    }),
                    right: Some(SplitCell {
                        no: line.new_no,
                        text: clean(&line.text),
                        changed: false,
                    }),
                });
            }
        }
    }
    flush(&mut rows, &mut rem, &mut add);
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::git_diff::{DiffLine, LineKind};

    fn line(kind: LineKind, old: Option<u32>, new: Option<u32>, text: &str) -> DiffLine {
        DiffLine {
            kind,
            old_no: old,
            new_no: new,
            text: text.to_string(),
        }
    }

    /// An uneven replacement (2 removed ↔ 1 added) between two context lines:
    /// the pair aligns positionally, the extra removed line pairs with an empty
    /// right column, and context lines land identically on both sides.
    #[test]
    fn pairs_removed_and_added_side_by_side() {
        let lines = vec![
            line(LineKind::Context, Some(1), Some(1), "a"),
            line(LineKind::Removed, Some(2), None, "b"),
            line(LineKind::Removed, Some(3), None, "c"),
            line(LineKind::Added, None, Some(2), "B"),
            line(LineKind::Context, Some(4), Some(3), "d"),
        ];
        let rows = split_hunk(&lines);
        assert_eq!(rows.len(), 4);

        // Leading context: same text both sides, not tinted.
        let l = rows[0].left.as_ref().unwrap();
        let r = rows[0].right.as_ref().unwrap();
        assert_eq!((l.no, l.text.as_str(), l.changed), (Some(1), "a", false));
        assert_eq!((r.no, r.text.as_str(), r.changed), (Some(1), "a", false));

        // First changed row: removed[0] ↔ added[0], both tinted.
        let l = rows[1].left.as_ref().unwrap();
        let r = rows[1].right.as_ref().unwrap();
        assert_eq!((l.no, l.text.as_str(), l.changed), (Some(2), "b", true));
        assert_eq!((r.no, r.text.as_str(), r.changed), (Some(2), "B", true));

        // Leftover removed line pairs with an empty right column.
        assert_eq!(rows[2].left.as_ref().unwrap().text, "c");
        assert!(rows[2].right.is_none());

        // Trailing context resumes both columns.
        assert_eq!(rows[3].left.as_ref().unwrap().no, Some(4));
        assert_eq!(rows[3].right.as_ref().unwrap().no, Some(3));
    }

    /// Tabs render as four spaces so indentation survives UI text layout.
    #[test]
    fn expands_tabs_in_cell_text() {
        let lines = vec![line(LineKind::Added, None, Some(1), "\tindented")];
        let rows = split_hunk(&lines);
        assert_eq!(rows[0].right.as_ref().unwrap().text, "    indented");
        assert!(rows[0].left.is_none());
    }
}

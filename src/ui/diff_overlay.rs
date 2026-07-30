//! The working-tree diff overlay: a read-only, GitHub-style side-by-side diff
//! that covers the terminal area when the user clicks a sidebar row's git line
//! (`⎇ branch +N −N`) or a Changes-panel row. A scrolling column of per-file
//! cards with collapsible hunk bodies — old on the left, new on the right —
//! plus an untracked-files section `git diff` itself can't show.
//!
//! The sidebar half of that is opt-out: everything below assumes the git line
//! is a click target, which it is unless
//! [`sidebar_diff_preview`](crate::core::config::Config::sidebar_diff_preview)
//! is off. The panel row is not gated.
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
//!
//! One probe, one snapshot, however many watchers: every tab's overlay and the
//! Changes panel go through
//! [`spawn_shared_diff_probe`](Tty7App::spawn_shared_diff_probe) and hold the
//! result behind an `Arc`. The element tree, meanwhile, is *not* virtualized —
//! so what keeps a big working tree from stalling the window is refusing to
//! build the rows in the first place: past
//! [`AUTO_COLLAPSE_TOTAL_LINES`](git_diff::AUTO_COLLAPSE_TOTAL_LINES) every
//! file opens collapsed under a summary, and at most
//! [`MAX_RENDERED_FILES`] cards are built at all.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{
    AnyElement, FocusHandle, FontWeight, KeyDownEvent, Pixels, Window, div, prelude::*, px,
};
use gpui_component::button::Button;
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};

use crate::terminal::git_diff::{
    self, AUTO_COLLAPSE_LINES, DiffSnapshot, DiffStats, FileDiff, FileStatus, LineKind,
    MAX_RENDERED_FILES, Truncation,
};
use crate::ui::app::Tty7App;
use crate::ui::rounding;
use crate::ui::rounding::RoundedCorners as _;

/// What the overlay currently shows: probing, a parsed snapshot, or the
/// answer that the cwd stopped being a repo.
pub(crate) enum DiffLoad {
    /// First probe still in flight.
    Loading,
    /// A landed snapshot, shared rather than owned: one probe result reaches
    /// every tab whose overlay watches this cwd *and* the Changes panel, and
    /// the snapshot is a deep tree of owned strings — cloning it per holder on
    /// the UI update path is exactly the cost issue #239 measured as a stall.
    Ready(Arc<DiffSnapshot>),
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
    /// Files the user has explicitly expanded (`true`) or collapsed (`false`),
    /// keyed by path so the choice survives a background refresh of the
    /// snapshot. Absent means "follow the default".
    ///
    /// Absolute state, deliberately not an inversion set. It used to be a
    /// `HashSet` of "files flipped away from their default", which was fine
    /// while the default was per-file and stable — but the repo-wide
    /// `collapse_all` moves the default for *every* file at once, so a refresh
    /// that crossed the oversized threshold inverted every explicit choice
    /// simultaneously: the two files the user had opened snapped shut and the
    /// rest sprang open. Storing what the user actually wanted makes a moving
    /// default unable to touch it.
    pub(crate) expanded: HashMap<String, bool>,
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
        // The Changes panel may already hold this very repo's snapshot — it is
        // the same `git diff HEAD`. Opening on it makes the overlay paint
        // immediately instead of flashing "Reading diff…" for a probe whose
        // answer is already in the process, and costs an `Arc` bump. A refresh
        // probe still flies below, so the seeded view is never the last word.
        // Read here, before the `&mut` borrow of the tab.
        let seed = match (&self.right_panel.diff_cwd, &self.right_panel.diff) {
            (Some(panel_key), Some(Some(snap))) if *panel_key == (host, cwd.clone()) => {
                DiffLoad::Ready(Arc::clone(snap))
            }
            _ => DiffLoad::Loading,
        };
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
            load: seed,
            loading: false,
            expanded: HashMap::new(),
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
        self.spawn_shared_diff_probe(host, cwd, cx);
    }

    /// One `git diff HEAD` per repository, however many things are waiting on
    /// it — where "repository" is the machine *and* the path, since the same
    /// path on two hosts is two different work trees.
    ///
    /// The overlay and the Changes panel used to probe the same repository
    /// independently and each keep its own `DiffSnapshot` — issue #239's fifth
    /// finding. Deduping here means opening both costs one invocation and one
    /// parse, and [`install_diff_snapshot`](Self::install_diff_snapshot) hands
    /// the *same* `Arc` to both rather than a second copy.
    ///
    /// Callers still mark themselves as waiting first (the overlay's `loading`
    /// flag, the panel's `diff_pending`): that's the "refreshing…" hint, and it
    /// is cleared by whichever probe lands for this repo, not necessarily the
    /// one the caller thought it started.
    pub(crate) fn spawn_shared_diff_probe(
        &mut self,
        host: crate::ui::host_ops::SharedHost,
        cwd: PathBuf,
        cx: &mut Context<Self>,
    ) {
        let key = (host.id(), cwd.clone());
        if !self.diff_probes_inflight.insert(key.clone()) {
            // Someone is already asking this exact question — but they asked it
            // *earlier*, and the answer in flight describes the tree as it was
            // then. This caller only got here because something changed since,
            // so folding it into that request would hand it a snapshot already
            // known to be stale and leave nothing to trigger another look: the
            // overlay's own re-check is gated on `loading`, which the landing
            // clears. Remember to ask again instead.
            self.diff_probes_restale.insert(key);
            return;
        }
        let host_for_retry = host.clone();
        let probe_cwd = cwd.clone();
        crate::ui::host_ops::HostOps::run(
            host,
            cx,
            move |h| git_diff::probe(h, &probe_cwd),
            move |app, result, cx| {
                app.diff_probes_inflight.remove(&key);
                app.install_diff_snapshot(key.0, &cwd, result.map(Arc::new), cx);
                // Re-ask for whoever was folded in above. Cleared first, so the
                // fresh probe starts with a clean slate and a request that
                // arrives while *it* flies marks the flag again — this converges
                // rather than looping, because a quiet tree never sets it.
                if app.diff_probes_restale.remove(&(key.0, cwd.clone())) {
                    app.spawn_shared_diff_probe(host_for_retry, cwd, cx);
                }
            },
        );
    }

    /// Hand a landed probe to everything watching `cwd`: every tab whose
    /// overlay shows it (the spawning tab may no longer be active, and sibling
    /// tabs on the same repo are equally stale) and the Changes panel when it
    /// is on the same cwd. Slots that closed or swapped repos while the probe
    /// flew are skipped.
    fn install_diff_snapshot(
        &mut self,
        host: crate::ui::host_ops::HostId,
        cwd: &Path,
        snap: Option<Arc<DiffSnapshot>>,
        cx: &mut Context<Self>,
    ) {
        let mut landed = false;
        for tab in self.tabs.iter_mut() {
            let Some(overlay) = tab
                .diff_overlay
                .as_mut()
                .filter(|o| o.cwd == cwd && o.host_id == host)
            else {
                continue;
            };
            overlay.loading = false;
            // `Arc::clone`, not a deep copy of the file/hunk/line tree.
            overlay.load = match &snap {
                Some(snap) => DiffLoad::Ready(Arc::clone(snap)),
                None => DiffLoad::NotARepo,
            };
            landed = true;
        }
        // The panel's *wait* is cleared by the answer it asked for, whoever
        // actually ran it — that's what `diff_pending` is for, and clearing it
        // on a result for a repo the panel has since left is what lets the
        // render path notice nothing is cached and re-probe.
        let key = (host, cwd.to_path_buf());
        if self.right_panel.diff_pending.as_ref() == Some(&key) {
            self.right_panel.diff_pending = None;
            landed = true;
        }
        // The panel's *data*, though, is claimed by the repo key alone.
        // Requiring the panel to have been the one waiting meant a probe the
        // overlay started was thrown away for the panel even when it was
        // sitting on that exact repo — so clicking the sidebar counts left the
        // overlay showing the new snapshot and the panel still rendering the
        // old one, in the same window. With probes deduped per repo there is at
        // most one in flight, so there is no out-of-order overwrite to guard
        // against.
        if self.right_panel.diff_cwd.as_ref() == Some(&key) {
            self.right_panel.diff = Some(snap);
            landed = true;
        }
        if landed {
            cx.notify();
        }
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
    pub(crate) fn render_diff_overlay(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let overlay = self.tabs.get(self.active)?.diff_overlay.as_ref()?;

        let content = match &overlay.load {
            DiffLoad::Loading => self.diff_message("Reading diff…", cx),
            DiffLoad::NotARepo => self.diff_message("Not a git repository", cx),
            // Empty because the read broke, not because the tree is clean. Both
            // land here as a snapshot with no files — see
            // `DiffSnapshot::read_failed` — and only one of them may be reported
            // as a fact about the repository.
            DiffLoad::Ready(snap) if empty_snapshot(snap) && snap.read_failed => self.diff_message(
                "Couldn't read the working-tree diff — retrying on the next refresh.",
                cx,
            ),
            DiffLoad::Ready(snap) if empty_snapshot(snap) => {
                self.diff_message("Working tree clean", cx)
            }
            DiffLoad::Ready(snap) => {
                self.diff_file_list(snap, &overlay.expanded, focused_file(snap, overlay), cx)
            }
        };

        let header = self.diff_header(overlay, window, cx);

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
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        // Through `stats` like every other whole-snapshot question on the render
        // path, rather than `totals` plus `untracked_count`: same single walk,
        // and it keeps "ask the snapshot once" a rule with no exceptions to
        // drift from.
        let (branch, files, untracked, added, removed) = match &overlay.load {
            DiffLoad::Ready(s) => {
                let stats = s.stats();
                let (a, r) = stats.totals;
                (s.branch.clone(), s.files.len(), stats.untracked_count, a, r)
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
        let row = crate::ui::app::title_bar_drag(
            h_flex().id("diff-overlay-header"),
            "diff-overlay-header",
            window,
            cx,
        );
        row.flex_shrink_0()
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
        expanded: &HashMap<String, bool>,
        focused: Option<usize>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // One walk for every whole-snapshot number this render needs, rather
        // than one walk per question over a file list whose length is the size
        // of the working tree — see `DiffSnapshot::stats`.
        let stats = snap.stats();
        // An oversized tree opens fully collapsed: the file rows are still the
        // useful part, and the bodies are what cost. A file the user opened by
        // name is exempt — that's an explicit request for one body, not for the
        // whole tree. See `DiffSnapshot::oversized`.
        let oversized = focused.is_none() && stats.oversized;
        let mut list = v_flex().gap_3().p_4().w_full();
        if oversized {
            list = list.child(self.diff_oversized_notice(snap, &stats, cx));
        }
        // Hard ceiling on cards built at all: even collapsed, one card per file
        // is one card per file, and the list is not virtualized.
        let shown = snap.files.len().min(MAX_RENDERED_FILES);
        for (idx, file) in snap.files.iter().enumerate() {
            if focused.is_some_and(|f| f != idx) {
                continue;
            }
            if focused.is_none() && idx >= shown {
                break;
            }
            // A file opened by name was asked for explicitly — show its body
            // even when it's over the auto-collapse threshold. The header still
            // toggles, so a huge file can be folded back down.
            let is_expanded = if focused == Some(idx) {
                expanded.get(&file.path).copied().unwrap_or(true)
            } else {
                file_expanded(file, expanded, oversized)
            };
            list = list.child(self.diff_file_card(idx, file, is_expanded, cx));
        }
        if focused.is_none() && snap.files.len() > shown {
            let rest = snap.files.len() - shown;
            list = list.child(
                div()
                    .w_full()
                    .px_2p5()
                    .py_1p5()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!(
                        "… and {rest} more changed file{} — run `git diff` in the terminal to see them.",
                        if rest == 1 { "" } else { "s" }
                    )),
            );
        }
        // Untracked files are a property of the tree, not of the focused file.
        if focused.is_none() && !snap.untracked.is_empty() {
            list = list.child(self.diff_untracked_section(snap, cx));
        }
        div()
            .id("diff-overlay-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .child(list)
            .into_any_element()
    }

    /// The banner an oversized diff leads with: says why every file is folded
    /// shut and points at the two ways out (expand one file, or use the
    /// terminal). Deliberately *above* the list rather than instead of it — the
    /// file rows with their `+N −N` are the part that still reads fine at this
    /// size.
    fn diff_oversized_notice(
        &self,
        snap: &DiffSnapshot,
        stats: &DiffStats,
        cx: &Context<Self>,
    ) -> AnyElement {
        let text = format!(
            "This working tree is too large to render efficiently ({}). Every file is \
             collapsed — expand individual files, or run `git diff` in the terminal.",
            oversized_summary(snap, stats),
        );
        div()
            .w_full()
            .px_2p5()
            .py_2()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().secondary)
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(text)
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
        // header is inert (no chevron, no click). A file the repo-wide budget
        // emptied *is* expandable even with no hunks — what it reveals is the
        // note explaining why, which is otherwise unreachable.
        let expandable =
            !file.binary && (!file.hunks.is_empty() || file.truncated == Some(Truncation::Budget));
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
        let has_body = expanded && (!file.hunks.is_empty() || file.truncated.is_some());

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
                            // Record what the user now wants, not "differs from
                            // the default": `expanded` is the state this card is
                            // currently drawn in, so the click means the
                            // opposite of it, and that answer keeps holding even
                            // if the default later moves under it.
                            overlay.expanded.insert(path.clone(), !expanded);
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
            let closing_row = if file.truncated.is_some() {
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
            if let Some(reason) = file.truncated {
                let note = match reason {
                    Truncation::PerFile => format!(
                        "Diff truncated at {} lines — run `git diff` in the terminal for the rest.",
                        git_diff::MAX_LINES_PER_FILE
                    ),
                    // Naming the repo-wide budget matters: this file may be
                    // three lines long, and "truncated" without a why reads as
                    // tty7 having lost the change.
                    Truncation::Budget => {
                        "Body not loaded — this working tree is past tty7's diff budget. \
                         Run `git diff` in the terminal for this file."
                            .to_string()
                    }
                };
                body = body.child(
                    div()
                        .w_full()
                        .px_2()
                        .py_1()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(note),
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
    ///
    /// Bounded exactly like the file-card list above it, and for the same
    /// reason: this is one non-virtualized row per path, and `ls-files
    /// --others` on a tree whose dependency directory isn't ignored yet answers
    /// with tens of thousands of them. The header count is the true total, so
    /// capping the rows never makes files look gone.
    fn diff_untracked_section(&self, snap: &DiffSnapshot, cx: &Context<Self>) -> AnyElement {
        let total = snap.untracked_count();
        let untracked = &snap.untracked[..snap.untracked.len().min(MAX_RENDERED_FILES)];
        // Same filled-band-in-a-rounded-card shape as `diff_file_card`, so the
        // header owns the corners it sits in. The rows below it paint no fill,
        // which is why only the top pair is ever non-zero here. Counted off the
        // *total*, not the capped slice: a section with a "… and N more" tail
        // still has rows under its header.
        let header_corners = rounding::stack_corners(
            0,
            if total == 0 { 1 } else { 2 },
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
                    .child(format!("Untracked files ({total})")),
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
        if total > untracked.len() {
            let rest = total - untracked.len();
            section = section.child(
                div()
                    .w_full()
                    .px_2p5()
                    .py_1()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!(
                        "… and {rest} more — run `git status` in the terminal to see them.",
                    )),
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

/// Nothing to show: no changed file and no untracked path. Says nothing about
/// *why* — [`DiffSnapshot::read_failed`] is what tells a clean tree apart from
/// a read that never landed.
fn empty_snapshot(snap: &DiffSnapshot) -> bool {
    snap.files.is_empty() && snap.untracked.is_empty()
}

/// Whether a file's body shows.
///
/// An explicit choice in `expanded` is final: it is answered before the default
/// is even computed, so nothing about the snapshot can change it. That ordering
/// is the whole point — `collapse_all` is a repo-wide default that moves as the
/// working tree grows and shrinks, and a user who opened one file inside an
/// oversized diff must not have it snap shut the moment an agent reverts enough
/// lines to drop the tree back under the threshold.
///
/// Files the user never touched follow the default: small text diffs open, big
/// ones closed, and past
/// [`AUTO_COLLAPSE_TOTAL_LINES`](git_diff::AUTO_COLLAPSE_TOTAL_LINES) nothing
/// opens at all, because the per-file threshold can't see that forty innocent
/// files are about to expand at once.
fn file_expanded(file: &FileDiff, expanded: &HashMap<String, bool>, collapse_all: bool) -> bool {
    if let Some(&want) = expanded.get(&file.path) {
        return want;
    }
    !collapse_all && file.added + file.removed <= AUTO_COLLAPSE_LINES
}

/// The parenthetical inside the oversized banner: one clause per axis that
/// contributes, so the banner never claims the *diff* is big when what is
/// actually big is an un-ignored untracked tree.
///
/// Whether hunks were dropped is answered by the parser's own per-file
/// [`Truncation`] flags, never by comparing the retained count against
/// `added + removed`. Those two numbers are scoped differently — retained
/// counts the context lines that get rendered too, `added + removed` doesn't —
/// so on a diff of many small hunks the retained figure is the *larger* of the
/// two and a `loaded < total` test reads a truncated tree as complete, while
/// the file cards below it say "body not loaded". The comparison has produced a
/// wrong answer on each axis it was ever asked about; it decides nothing here.
///
/// Both axes are named, and they compose: one file can hit the per-file cap in
/// the same snapshot where the repo-wide budget emptied another, and the banner
/// has to account for a body the reader can see is missing either way.
///
/// The changed-line figure stays [`totals`](DiffSnapshot::totals) exactly, so
/// the banner agrees with the `+N −N` in the header directly above it; the
/// retained figure is named as rendered rows rather than joined to it by "of",
/// because it is not a fraction of it.
fn oversized_summary(snap: &DiffSnapshot, stats: &DiffStats) -> String {
    let mut parts = vec![format!(
        "{} changed file{}",
        snap.files.len(),
        if snap.files.len() == 1 { "" } else { "s" }
    )];
    let (added, removed) = stats.totals;
    let total_lines = (added + removed) as usize;
    let loaded = stats.retained_lines;
    let budget = stats.budget_exhausted;
    let per_file = stats.per_file_truncated;
    parts.push(match (budget, per_file) {
        (false, false) => format!("{total_lines} diff lines"),
        _ => {
            let cap = match (budget, per_file) {
                (true, true) => "tty7's budget and the per-file cap",
                (true, false) => "tty7's budget",
                _ => "the per-file cap",
            };
            format!(
                "{total_lines} changed lines, {loaded} diff rows loaded before {cap} cut the rest"
            )
        }
    });
    if stats.untracked_count > 0 {
        parts.push(format!("{} untracked", stats.untracked_count));
    }
    parts.join(", ")
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

    /// A file of `added` changed lines, small enough to open by default on its
    /// own.
    fn small_file(path: &str, added: u32) -> FileDiff {
        FileDiff {
            path: path.to_string(),
            old_path: None,
            status: FileStatus::Modified,
            added,
            removed: 0,
            binary: false,
            truncated: None,
            hunks: vec![git_diff::Hunk {
                header: "@@ -1,1 +1,1 @@".to_string(),
                lines: (0..added)
                    .map(|i| line(LineKind::Added, None, Some(i + 1), "x"))
                    .collect(),
            }],
        }
    }

    /// A file whose body is mostly context: one changed line under six context
    /// lines, so `retained_lines` counts seven where `+N −N` counts one. A tree
    /// of these is the shape where the retained figure *exceeds* the changed
    /// one, which is where a `loaded < total` comparison reads truncation
    /// backwards.
    fn context_heavy_file(path: &str) -> FileDiff {
        FileDiff {
            path: path.to_string(),
            old_path: None,
            status: FileStatus::Modified,
            added: 1,
            removed: 0,
            binary: false,
            truncated: None,
            hunks: vec![git_diff::Hunk {
                header: "@@ -1,7 +1,7 @@".to_string(),
                lines: (0..6)
                    .map(|i| line(LineKind::Context, Some(i + 1), Some(i + 1), "ctx"))
                    .chain(std::iter::once(line(LineKind::Added, None, Some(7), "x")))
                    .collect(),
            }],
        }
    }

    /// An explicit choice map, for the tests below.
    fn choices<const N: usize>(pairs: [(&str, bool); N]) -> HashMap<String, bool> {
        pairs.into_iter().map(|(p, v)| (p.to_string(), v)).collect()
    }

    /// The banner text for a snapshot, deriving its stats the way the render
    /// path does — so these tests exercise the same numbers the overlay shows
    /// rather than a hand-assembled set.
    fn banner(snap: &DiffSnapshot) -> String {
        oversized_summary(snap, &snap.stats())
    }

    /// The per-file threshold on its own: a small file opens, a big one doesn't,
    /// and an explicit choice overrides either. Unchanged behaviour — this is
    /// the small-working-tree case that must feel identical.
    #[test]
    fn per_file_collapse_is_unchanged_below_the_repo_threshold() {
        let small = small_file("small.rs", 10);
        let big = small_file("big.rs", AUTO_COLLAPSE_LINES + 1);
        let none = HashMap::new();
        assert!(file_expanded(&small, &none, false));
        assert!(!file_expanded(&big, &none, false));

        let picked = choices([("small.rs", false), ("big.rs", true)]);
        assert!(!file_expanded(&small, &picked, false));
        assert!(file_expanded(&big, &picked, false));
    }

    /// The repo-wide override: past the total threshold nothing opens by
    /// default, however small each file is — the case a per-file rule can't see
    /// (issue #239, finding 4). An explicit choice still wins, which is what
    /// "expand individual files" in the oversized notice means.
    #[test]
    fn repo_wide_collapse_overrides_the_per_file_default() {
        let small = small_file("small.rs", 10);
        let none = HashMap::new();
        assert!(file_expanded(&small, &none, false));
        assert!(!file_expanded(&small, &none, true), "collapsed en masse");

        assert!(
            file_expanded(&small, &choices([("small.rs", true)]), true),
            "the user's own click still opens it"
        );
    }

    /// An explicit choice must survive the default moving under it — the defect
    /// the inversion-set representation had. A refresh that crosses the
    /// oversized threshold in either direction leaves every file the user
    /// touched exactly as they left it, and moves only the ones they didn't.
    #[test]
    fn explicit_choices_survive_an_oversized_transition() {
        let opened = small_file("opened.rs", 10);
        let closed = small_file("closed.rs", 10);
        let untouched = small_file("untouched.rs", 10);
        // The user opened one file and shut another while the tree was oversized.
        let picked = choices([("opened.rs", true), ("closed.rs", false)]);

        for collapse_all in [true, false] {
            assert!(
                file_expanded(&opened, &picked, collapse_all),
                "an explicitly opened file stays open (collapse_all={collapse_all})"
            );
            assert!(
                !file_expanded(&closed, &picked, collapse_all),
                "an explicitly closed file stays closed (collapse_all={collapse_all})"
            );
        }
        // Only the file the user never touched follows the default.
        assert!(!file_expanded(&untouched, &picked, true));
        assert!(file_expanded(&untouched, &picked, false));
    }

    /// Sixty files of a hundred and fifty lines each never trip the per-file
    /// threshold (each is well under it), yet would open 9000 diff rows at once.
    /// `oversized` catches it on the line axis, and with everything collapsed
    /// the overlay builds zero rows.
    #[test]
    fn many_medium_files_are_oversized_and_build_no_rows() {
        let snap = DiffSnapshot {
            files: (0..60)
                .map(|i| small_file(&format!("f{i}.rs"), 150))
                .collect(),
            ..Default::default()
        };
        assert!(
            snap.files.iter().all(|f| f.added <= AUTO_COLLAPSE_LINES),
            "no single file is over the per-file threshold"
        );
        assert!(snap.stats().oversized);

        let none = HashMap::new();
        let rows_expanded: usize = snap
            .files
            .iter()
            .filter(|f| file_expanded(f, &none, false))
            .flat_map(|f| &f.hunks)
            .map(|h| split_hunk(&h.lines).len())
            .sum();
        let rows_collapsed: usize = snap
            .files
            .iter()
            .filter(|f| file_expanded(f, &none, true))
            .flat_map(|f| &f.hunks)
            .map(|h| split_hunk(&h.lines).len())
            .sum();
        assert_eq!(rows_expanded, 9000, "what the old rule would have built");
        assert_eq!(rows_collapsed, 0);
    }

    /// The other side of the same coin: a busy-but-ordinary afternoon is *not*
    /// oversized and opens expanded exactly as it does today. The thresholds
    /// must not tax a normal working tree.
    ///
    /// Built out of context-heavy files rather than bare changed lines, because
    /// that is what a real diff looks like and it is the difference the line
    /// threshold is most easily mis-set against: git prints three lines of
    /// context each side of every hunk, so `retained_lines` runs several times
    /// the `+N −N` a person reads off the header. A tree of forty files with a
    /// handful of small hunks each is an afternoon's work, and it must open.
    #[test]
    fn an_ordinary_busy_tree_is_not_oversized() {
        // Forty files × ten hunks × (6 context + 1 changed) — 2800 retained
        // lines behind a header reading `+400 −0`, which is a morning, not a
        // refactor. Sized to sit above the threshold this used to carry and
        // below the one it carries now: an assertion that passes either way
        // would not be watching anything.
        let snap = DiffSnapshot {
            files: (0..40)
                .map(|i| {
                    let mut f = context_heavy_file(&format!("f{i}.rs"));
                    f.hunks = std::iter::repeat_n(f.hunks[0].clone(), 10).collect();
                    f.added = 10;
                    f
                })
                .collect(),
            ..Default::default()
        };
        let (added, removed) = snap.totals();
        assert!(
            snap.stats().retained_lines > (added + removed) as usize * 4,
            "the context lines dominate, as they do in a real diff"
        );
        assert!(
            !snap.stats().oversized,
            "an ordinary afternoon must not read as a tree too large to render \
             ({} retained lines)",
            snap.stats().retained_lines
        );
        let none = HashMap::new();
        assert!(snap.files.iter().all(|f| file_expanded(f, &none, false)));
    }

    /// An empty snapshot means one of two opposite things, and the overlay has
    /// to tell them apart before it says either out loud.
    ///
    /// "Working tree clean" is a claim about the repository. A probe that could
    /// not run — a refused stream, a read that went silent, a git racing a
    /// concurrent write — produces exactly the same empty file list, and saying
    /// it there tells someone their changes are gone.
    #[test]
    fn an_empty_snapshot_reads_as_clean_only_when_the_probe_worked() {
        let clean = DiffSnapshot {
            branch: "main".into(),
            ..Default::default()
        };
        assert!(empty_snapshot(&clean));
        assert!(!clean.read_failed, "nothing went wrong: this tree is clean");

        let broken = DiffSnapshot {
            branch: "main".into(),
            read_failed: true,
            ..Default::default()
        };
        assert!(
            empty_snapshot(&broken),
            "indistinguishable by shape — which is the point"
        );
        assert!(broken.read_failed, "and distinguishable by this");

        // A read that failed *after* producing something is not the empty case
        // at all: the file list renders, and no claim about emptiness is made.
        let partial = DiffSnapshot {
            files: vec![small_file("one.rs", 3)],
            read_failed: true,
            ..Default::default()
        };
        assert!(!empty_snapshot(&partial));
    }

    /// A huge untracked list does *not* collapse the diff — and must not.
    ///
    /// It is the same one-row-per-entry cost, but `oversized` is not the lever
    /// that answers it: folding every file body shut leaves the untracked
    /// section rendering exactly as many rows as before, because that section
    /// has no bodies to fold. Driving it from here meant a tree with an
    /// un-ignored `node_modules` and three edited files hid the three cheap
    /// things, kept the expensive one, and told the reader their working tree
    /// was too large to render. What actually bounds it is the retention cap and
    /// the row cap, asserted below.
    #[test]
    fn a_huge_untracked_list_does_not_collapse_the_diff() {
        let snap = DiffSnapshot {
            files: vec![small_file("one.rs", 3)],
            untracked: (0..git_diff::MAX_UNTRACKED)
                .map(|i| format!("node_modules/p{i}/index.js"))
                .collect(),
            untracked_total: 40_000,
            ..Default::default()
        };
        assert!(snap.stats().retained_lines < git_diff::AUTO_COLLAPSE_TOTAL_LINES);
        assert!(snap.files.len() < git_diff::AUTO_COLLAPSE_TOTAL_FILES);
        assert!(
            !snap.stats().oversized,
            "collapsing the diff would not have removed a single untracked row"
        );

        // The bound that does apply, on the rows that are actually expensive.
        assert_eq!(
            snap.untracked.len(),
            git_diff::MAX_UNTRACKED,
            "retention is capped at the parser"
        );
        assert_eq!(
            snap.untracked.len().min(MAX_RENDERED_FILES),
            MAX_RENDERED_FILES,
            "and rows at the renderer"
        );
        assert_eq!(
            snap.untracked_count(),
            40_000,
            "while the reported count stays the true total"
        );
    }

    /// The untracked section builds at most [`MAX_RENDERED_FILES`] rows while
    /// still reporting the true total, so a capped list never reads as files
    /// having vanished.
    #[test]
    fn untracked_rows_are_capped_but_the_count_stays_true() {
        let snap = DiffSnapshot {
            untracked: (0..git_diff::MAX_UNTRACKED)
                .map(|i| format!("p{i}"))
                .collect(),
            untracked_total: 12_345,
            ..Default::default()
        };
        let rendered = snap.untracked.len().min(MAX_RENDERED_FILES);
        assert_eq!(rendered, MAX_RENDERED_FILES);
        assert_eq!(snap.untracked_count(), 12_345, "header count is honest");
        assert_eq!(snap.untracked_count() - rendered, 12_045);
    }

    /// A snapshot built without the streaming probe (tests, `..Default`) must
    /// not under-report: the count falls back to the retained length.
    #[test]
    fn untracked_count_falls_back_to_the_retained_length() {
        let snap = DiffSnapshot {
            untracked: vec!["a".into(), "b".into(), "c".into()],
            ..Default::default()
        };
        assert_eq!(snap.untracked_total, 0);
        assert_eq!(snap.untracked_count(), 3);
    }

    /// However many files change, the overlay builds at most
    /// [`MAX_RENDERED_FILES`] cards and says so.
    #[test]
    fn file_cards_are_capped() {
        let snap = DiffSnapshot {
            files: (0..MAX_RENDERED_FILES + 25)
                .map(|i| small_file(&format!("f{i}.rs"), 1))
                .collect(),
            ..Default::default()
        };
        let shown = snap.files.len().min(MAX_RENDERED_FILES);
        assert_eq!(shown, MAX_RENDERED_FILES);
        assert_eq!(
            snap.files.len() - shown,
            25,
            "the tail gets one summary line"
        );
    }

    /// The shape the old `loaded < total_lines` test could not see: many
    /// one-line hunks, each carrying its context. Retained lines count context
    /// and `+N −N` does not, so here the retained figure is the *larger* of the
    /// two — a budget-truncated tree that the comparison read as complete,
    /// leaving the banner silent while the file cards below it said "body not
    /// loaded". The banner must name the budget whenever any file carries
    /// [`Truncation::Budget`], whatever the diff's shape.
    #[test]
    fn the_banner_names_the_budget_when_context_outweighs_the_changes() {
        let files: Vec<FileDiff> = (0..200)
            .map(|i| context_heavy_file(&format!("f{i}.rs")))
            .collect();

        // Same tree, one file whose body the repo-wide budget dropped.
        let mut truncated = files.clone();
        truncated.push(FileDiff {
            hunks: vec![],
            truncated: Some(Truncation::Budget),
            ..context_heavy_file("dropped.rs")
        });
        let snap = DiffSnapshot {
            files: truncated,
            ..Default::default()
        };
        let (added, removed) = snap.totals();
        let total = (added + removed) as usize;
        assert!(
            snap.stats().retained_lines > total,
            "the context lines outweigh the changed ones — the shape that slipped through"
        );
        assert!(snap.stats().budget_exhausted);

        let summary = banner(&snap);
        assert!(summary.contains("budget"), "{summary}");
        assert!(
            summary.contains(&format!("{total} changed lines")),
            "the exact `+N −N` from the header, not the retained count: {summary}"
        );

        // The same shape with nothing dropped must not claim a truncation.
        let whole = DiffSnapshot {
            files,
            ..Default::default()
        };
        assert!(!whole.stats().budget_exhausted);
        assert!(!banner(&whole).contains("budget"));
    }

    /// The sibling axis, blind in exactly the same place: one file cut at
    /// [`MAX_LINES_PER_FILE`](git_diff::MAX_LINES_PER_FILE) inside a
    /// context-heavy tree. `loaded < total_lines` is false here — the context
    /// lines of every other file more than cover the cut one — so the branch
    /// that used to carry this clause stayed silent while that file's own card
    /// read "Diff truncated at 2000 lines". Both axes now come from the
    /// parser's flags, and a snapshot carrying both must name both.
    #[test]
    fn the_banner_names_the_per_file_cap_when_context_outweighs_the_changes() {
        let files: Vec<FileDiff> = (0..200)
            .map(|i| context_heavy_file(&format!("f{i}.rs")))
            .collect();

        let mut cut = files.clone();
        cut.push(FileDiff {
            truncated: Some(Truncation::PerFile),
            ..context_heavy_file("huge.rs")
        });
        let snap = DiffSnapshot {
            files: cut,
            ..Default::default()
        };
        let (added, removed) = snap.totals();
        let total = (added + removed) as usize;
        assert!(
            snap.stats().retained_lines > total,
            "the shape the comparison reads backwards"
        );
        assert!(
            !snap.stats().budget_exhausted,
            "the budget axis is not what fired"
        );

        let summary = banner(&snap);
        assert!(summary.contains("per-file cap"), "{summary}");
        assert!(
            summary.contains(&format!("{total} changed lines")),
            "the exact `+N −N` from the header, not the retained count: {summary}"
        );

        // Nothing cut, nothing claimed.
        let whole = DiffSnapshot {
            files: files.clone(),
            ..Default::default()
        };
        assert!(!banner(&whole).contains("per-file"));

        // Both kinds in one snapshot: neither clause may mask the other.
        let mut both = files;
        both.push(FileDiff {
            truncated: Some(Truncation::PerFile),
            ..context_heavy_file("huge.rs")
        });
        both.push(FileDiff {
            hunks: vec![],
            truncated: Some(Truncation::Budget),
            ..context_heavy_file("dropped.rs")
        });
        let summary = banner(&DiffSnapshot {
            files: both,
            ..Default::default()
        });
        assert!(summary.contains("budget"), "{summary}");
        assert!(summary.contains("per-file cap"), "{summary}");
    }

    /// Measurement harness for issue #239, finding 2 — run with
    /// `cargo test -- --ignored --nocapture bench_snapshot_share`.
    #[test]
    #[ignore = "measurement, not an assertion"]
    fn bench_snapshot_share() {
        use std::time::Instant;

        // Two sizes: what v26.7.5 would have held for a big agent session
        // (300 files × 300 lines, no repo-wide budget), and what this build
        // retains for the same tree once the budget applies.
        for (label, files, per_file) in [
            ("unbudgeted (v26.7.5 shape)", 300, 300),
            ("budgeted (this build retains)", 300, 67),
        ] {
            let snap = Arc::new(DiffSnapshot {
                files: (0..files)
                    .map(|i| small_file(&format!("f{i}.rs"), per_file))
                    .collect(),
                ..Default::default()
            });
            let lines: usize = snap.stats().retained_lines;

            let t = Instant::now();
            for _ in 0..10 {
                let _deep = (*snap).clone();
            }
            let deep = t.elapsed() / 10;

            let t = Instant::now();
            for _ in 0..100 {
                let _shared = Arc::clone(&snap);
            }
            let shared = t.elapsed() / 100;
            println!(
                "{label}: {files} files / {lines} lines — deep clone {deep:?} vs \
                 Arc::clone {shared:?}, per holder on the UI thread"
            );
        }
    }
}

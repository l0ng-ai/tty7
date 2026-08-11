//! The source control panel body: four groups of file rows over the working
//! tree status, in the order git itself talks about them.
//!
//! The panel is rendered from `WorkingTreeStatus`, which knows the difference
//! between the index and the working tree. That is the whole reason the old
//! flat list had to go: it ran one `git diff HEAD`, so it could not tell a
//! staged change from an unstaged one and showed every row the letter `M`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{AnyElement, Context, Focusable as _, SharedString, Window, div, prelude::*, px};
use gpui_component::button::{Button, ButtonCustomVariant, ButtonVariants as _};
use gpui_component::input::{Input, InputState};
use gpui_component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, h_flex, v_flex,
};

use tty7_core::core::git::diff::MAX_RENDERED_FILES;
use tty7_core::core::git::ops::GitOp;
use tty7_core::core::git::status::{
    ChangeCode, DecoStatus, HeadState, RepoOperation, RepoPath, StatusEntry, WorkingTreeStatus,
};

use crate::terminal::git_data::status_of;
use crate::terminal::git_diff::DiffSource;
use crate::ui::app::{CONTENT_INSET, TILE_GLYPH_XS, TILE_SIZE_XS, Tty7App};
use crate::ui::host_ops::{HostId, SharedHost};
use crate::ui::i18n::{L10nKey, t, t_fmt, t_plural};
use crate::ui::right_panel::{SEARCH_H, git_badge, info_chip};
use crate::ui::rounding::{CARD_RADIUS, HAIRLINE, RoundedCorners as _, segment_corners};
use crate::ui::scm::ScmIntent;
use crate::ui::scm::path::{elide_middle, split_display_path};
use crate::ui::scm::state::{RepoKey, ScmGroup};
use crate::ui::scm::status::{status_color, status_glyph};

/// A file row, and the group header above it. Both 24px, so the list reads as
/// one grid rather than as headers with a list hanging off them.
///
/// It is also the height `scm/detail.rs` gives the changed-file rows it shows
/// for a commit — the two lists are the same list pointed at different trees,
/// and a reader who opens a commit must not feel the pitch change under them.
const ROW_H: f32 = 24.;

/// The status letter's column, from `git_badge`. The group chevron sits in a
/// box of exactly this width so the two line up in one column down the panel.
///
/// This is `right_panel::BADGE_W` spelled out where the header can read it,
/// and the test below pins the two together. They are one column, and a column
/// drawn from two numbers is a column that will eventually be drawn from two
/// different numbers.
const BADGE_W: f32 = 14.;

/// Rows are laid out inside this inset and then pad themselves back out, so a
/// hovered row's background is wider than its text on both sides.
///
/// The text lands on `CONTENT_INSET` whatever this is — the list subtracts it
/// outside the row and the row adds it back inside — so all this number sets
/// is how far the hover fill bleeds past the text. `scm/detail.rs` carries the
/// same pair for the same reason.
const ROW_INSET: f32 = 4.;

/// The key context the message box installs, and the one `ScmCommit` is
/// bound inside. The two are the same string on purpose: a binding whose
/// context nothing attaches is a binding that never fires.
pub(crate) const COMMIT_KEY_CONTEXT: &str = "ScmCommit";

/// Past this many, the branch switcher scrolls instead of growing.
const BRANCHES_IN_MENU: usize = 12;

/// How much of a branch name survives the row. Long names carry their
/// meaning at both ends (`feature/…/auth-retry`), so the middle is what goes.
const BRANCH_NAME_CHARS: usize = 24;

/// Untracked files past this many start folded. A fresh clone of a repository
/// with a stale `.gitignore` can put thousands of them in front of the three
/// changes the user came to look at.
const UNTRACKED_AUTO_COLLAPSE: usize = 20;

/// The message box at rest, the ceiling it grows to, and the padding inside
/// its fill.
///
/// The box is a soft rounded fill with no outline, which is what
/// `switcher.rs`'s inline rename field already does and the only shape in the
/// app for "an input you write into rather than filter with". Every `Input` in
/// tty7 is `.appearance(false)`; a hairline here, and an accent ring on top of
/// it when focused, made the commit box the one outlined thing on a panel that
/// otherwise separates by surface and space alone. The fill it wears instead is
/// [`field_fill`], deliberately below the ramp's first rung.
///
/// The box still has to end up exactly [`SEARCH_H`] tall, which is what makes
/// every input row in the panel sit on one line, and getting there is
/// arithmetic rather than a guess:
///
/// * gpui-component lays an `Input`'s text out at a fixed `Rems(1.25)` line
///   height, which at the default 16px rem is `MSG_LINE` = 20px whatever size
///   the field is set to.
/// * At `.xsmall()` the field adds no vertical padding of its own — `input_py`
///   is 0 there — so one row of field measures exactly `MSG_LINE`.
/// * gpui measures border-box. With the hairline gone its 1px each side has to
///   come out of the padding or the box would shrink to 28: `5 + 20 + 5` = 30,
///   and the assertion below refuses to compile if that stops matching the
///   search strip.
///
/// `MSG_PAD_X` absorbs the same lost pixel — 9 against `input_px`'s 4 at
/// `.xsmall()`, and the two paddings add to the thirteen the hairline version
/// also reached, which is where the panel's text column is.
const MSG_LINE: f32 = 20.;
const MSG_PAD_X: f32 = 9.;
const MSG_PAD_Y: f32 = 5.;
const MSG_MIN_H: f32 = MSG_PAD_Y + MSG_LINE + MSG_PAD_Y;
/// The ceiling, as `MSG_ROWS_MAX` bare lines. It is a rail, not a border-box
/// sum: the wrapper's own hairline and padding are not in it, so at the very
/// top of the box's growth the last row is clipped rather than framed.
const MSG_MAX_H: f32 = MSG_ROWS_MAX as f32 * MSG_LINE;

/// The invariant the comment above describes, made unbreakable: the resting
/// message box and the panel's search strip are the same height, or this does
/// not build.
const _: () = assert!(MSG_MIN_H == SEARCH_H);

/// One line at rest and it grows into the message. The box is one row in a
/// column of rows, and a box that stands four lines tall before anything has
/// been typed pushes the file list — the thing the panel is for — off the
/// bottom of a 260px-wide sidebar.
const MSG_ROWS: usize = 1;
const MSG_ROWS_MAX: usize = 6;

/// The commit control: one frame, the label button and the chevron inside it,
/// and the glyph in the chevron.
///
/// A joined split control, sized like the panel's other controls rather than
/// stretched across it — the row it sits in reads "N staged" on the left and
/// offers this on the right, so it only has to be as wide as its label.
///
/// The frame is a hairline around a 22px interior, which puts it at 24 —
/// gpui measures border-box, and the hairline is part of what a reader sees.
/// Twenty-four is the panel's row pitch, so the control sits on the same line
/// grid as everything above and below it.
///
/// The chevron half is a 22px cell, so the divider falls where the eye expects
/// it rather than a couple of pixels early.
///
/// `COMMIT_GLYPH` is the group chevron's size: a caret is an affordance, not
/// content — it says "there is more here", and it says it at the same size as
/// every other small mark in the panel.
const COMMIT_H: f32 = 24.;
const COMMIT_CHEVRON_W: f32 = 22.;
const COMMIT_GLYPH: f32 = 11.;

/// How long to wait before asking git again about a directory that answered
/// with nothing.
///
/// `scm_refresh` is safe to call every frame — it de-duplicates in-flight
/// probes and skips fresh ones. What it cannot do is notice that a probe came
/// back empty: a repository we never got a status for stays stale forever, so
/// without this the panel would start a new `git status` on every frame.
const PROBE_RETRY: Duration = Duration::from_secs(2);

/// How long "there is no repository here" is believed for. Long enough that
/// sitting in `/tmp` costs nothing, short enough that `git init` in the pane
/// below shows up without touching anything.
const NOT_A_REPO_RETRY: Duration = Duration::from_secs(10);

/// What the panel knows about the directory the active pane is sitting in.
enum RepoLookup {
    /// Nothing has answered yet — the tab's own probe is still out.
    Pending,
    NotARepo,
    Root(PathBuf),
}

impl Tty7App {
    pub(crate) fn render_panel_scm(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.scm_watch_status(cx);
        // An explicit repository pick should outlive a pane switch inside the
        // tab it was made on, and not a jump to a different tab.
        if self.scm.override_tab != Some(self.active) {
            self.scm.repo_override = None;
            self.scm.override_tab = None;
        }

        let Some((host, cwd)) = self.scm_pane_target(window, cx) else {
            let title = self.panel_title(t(L10nKey::PanelScmTitle), None, None, window, cx);
            let body = self.panel_empty(
                t(L10nKey::PanelNoWorkingDirectory),
                Some(t(L10nKey::PanelNoWorkingDirectoryHint)),
                cx,
            );
            return self.scm_shell(title, body);
        };

        let root = match self.scm_repo_root(&host, &cwd, cx) {
            RepoLookup::Pending => {
                let title = self.panel_title(t(L10nKey::PanelScmTitle), None, None, window, cx);
                let body = self.panel_empty(t(L10nKey::PanelLoading), None, cx);
                return self.scm_shell(title, body);
            }
            RepoLookup::NotARepo => {
                let title = self.panel_title(t(L10nKey::PanelScmTitle), None, None, window, cx);
                let body = self.panel_empty(
                    t(L10nKey::PanelNotAGitRepo),
                    Some(t(L10nKey::PanelNotAGitRepoHint)),
                    cx,
                );
                return self.scm_shell(title, body);
            }
            RepoLookup::Root(root) => root,
        };

        self.scm.repo = Some(RepoKey {
            host: host.id(),
            root,
        });
        // An override whose host has left the registry is dropped, not worked
        // around: falling back to the *pane's* host while keeping the
        // override's root would probe the wrong machine for that path and
        // cache the answer under a mismatched key.
        if self
            .scm
            .repo_override
            .as_ref()
            .is_some_and(|k| crate::ui::host_registry::HostRegistry::get(cx, k.host).is_none())
        {
            self.scm.repo_override = None;
            self.scm.override_tab = None;
        }
        // An explicit pick from the switcher wins over the pane's own
        // repository, so everything below reads through `active_repo`.
        let repo = self
            .scm
            .active_repo()
            .cloned()
            .expect("the pane's repository was just recorded");
        let host = match crate::ui::host_registry::HostRegistry::get(cx, repo.host) {
            Some(host) => host,
            None => host,
        };
        self.scm_probe(&host, &repo.root, cx);
        let Some(status) = self.scm_seen_status(repo.host, &repo.root, cx) else {
            let title = self.panel_title(t(L10nKey::PanelScmTitle), None, None, window, cx);
            let body = self.panel_empty(t(L10nKey::PanelLoading), None, cx);
            return self.scm_shell(title, body);
        };

        let count = (status.total_entries > 0).then(|| status.total_entries.to_string());
        let title = self.panel_title(t(L10nKey::PanelScmTitle), count, None, window, cx);

        let branch = self.scm_branch_row(&repo, &status, cx);
        let naming = self.scm_new_branch_row(&repo, cx);
        let commit = self.scm_commit_box(&repo, &status, window, cx);
        let buttons = self.scm_commit_buttons(&repo, &status, cx);
        let mut pinned = vec![branch];
        pinned.extend(naming);
        pinned.push(commit);
        pinned.push(buttons);
        // A commit's detail replaces the working tree's, so the two can never
        // be on screen claiming to be the same thing.
        let detail = self.scm.detail.clone();
        let body = match detail
            .as_ref()
            .and_then(|d| self.render_commit_detail(d, window, cx))
        {
            Some(body) => body,
            None if status.is_clean() => self.panel_empty(
                t(L10nKey::PanelNoChanges),
                Some(t(L10nKey::PanelNoChangesHint)),
                cx,
            ),
            None => self.scm_groups(&repo, &status, cx),
        };
        let history = self.render_graph_section(&repo, window, cx);
        self.scm_shell_full(title, pinned, body, history)
    }

    /// The repository line: which branch, how far from its upstream, and one
    /// button to close the gap.
    ///
    /// A row of its own rather than the title's trailing slot. Off macOS the
    /// tab tiles render *after* that slot, and a branch name is the elastic
    /// element here — it would be the first thing squeezed. `render_sftp_
    /// breadcrumb` sets the same precedent.
    fn scm_branch_row(
        &mut self,
        repo: &RepoKey,
        status: &WorkingTreeStatus,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.scm_load_branches(repo, cx);
        let mono = cx.theme().mono_font_family.clone();
        let theme = cx.theme();
        let (warning, muted, fg) = (theme.warning, theme.muted_foreground, theme.foreground);
        let detached = matches!(status.head, HeadState::Detached { .. });
        let busy = self.scm_network_busy(repo, cx);
        let others = self.scm_other_repos(repo);

        h_flex()
            .flex_none()
            .items_center()
            .gap(px(6.))
            // Taller than a file row, and what sets it is the `TILE_SIZE_SM`
            // sync tile at the end plus 2px of air — not the type, which is a
            // step under it.
            .h(px(28.))
            .pl(px(CONTENT_INSET))
            .pr(px(crate::ui::app::tile_trailing_inset_sm()))
            .child(
                Icon::empty()
                    .path("icons/git-branch.svg")
                    .size(px(12.))
                    .text_color(muted),
            )
            // The trigger is a `Button` because that is the one element the
            // dropdown trait is implemented for. `dropdown_caret` turns its
            // label row into `justify_between`, which is what puts the name on
            // the left and the chevron against the notes.
            //
            // The branch is the most important word on the row, and it earns
            // that by being in full-strength ink — not by being set larger
            // than the file names underneath it.
            .child(
                Button::new("scm-branch")
                    .ghost()
                    .xsmall()
                    .dropdown_caret(true)
                    .label(elide_middle(&head_label(&status.head), BRANCH_NAME_CHARS).to_string())
                    .flex_1()
                    .min_w(px(0.))
                    .h(px(20.))
                    .rounded(px(5.))
                    .text_color(fg)
                    .when(detached, |s| s.font_family(mono.clone()))
                    .dropdown_menu_with_anchor(
                        gpui::Anchor::TopLeft,
                        self.scm_branch_menu(repo, status, cx),
                    ),
            )
            .children(others.map(|count| branch_note(&format!("+{count}"), muted, &mono)))
            .when(detached, |this| {
                this.child(info_chip(
                    t(L10nKey::ScmDetached),
                    warning.opacity(0.16),
                    warning,
                    &mono,
                ))
            })
            .children(status.operation.map(|op| {
                info_chip(
                    t(operation_label(op)),
                    warning.opacity(0.16),
                    warning,
                    &mono,
                )
            }))
            // Armed-amend is a mode you can forget you are in, which puts it in
            // the same class as `detached` and a rebase in progress above — so
            // it gets their tint, not a chip of its own invention.
            .when(self.scm.amend, |this| {
                this.child(info_chip(
                    t(L10nKey::ScmAmendBadge),
                    warning.opacity(0.16),
                    warning,
                    &mono,
                ))
            })
            .children(
                tracking_chip(status.upstream.as_deref(), status.ahead_behind)
                    .map(|text| branch_note(&text, muted, &mono)),
            )
            .child(
                crate::ui::tab_strip::chrome_tile_sized(
                    Button::new("scm-sync").icon(if busy {
                        Icon::new(IconName::LoaderCircle)
                    } else {
                        Icon::empty().path("icons/git-sync.svg")
                    }),
                    crate::ui::app::TILE_SIZE_SM,
                    crate::ui::app::TILE_GLYPH_SM,
                    false,
                    cx,
                )
                .rounded_md()
                .disabled(busy)
                .tooltip(if status.upstream.is_some() {
                    t(L10nKey::ScmSync)
                } else {
                    t(L10nKey::ScmPublishBranch)
                })
                .on_click(cx.listener(|this, _, window, cx| {
                    this.run_scm_action(ScmIntent::Sync, window, cx);
                })),
            )
            .into_any_element()
    }

    /// Whether a network operation against this repository is still running.
    ///
    /// Asks the slot counter `run_git_op` claims from, which is the operation
    /// itself rather than a proxy for it: the epoch moves for a `.git` event
    /// or a file save too, so watching that would drop the spinner in the
    /// middle of a slow push.
    ///
    /// Read through `try_global`, never `default_global` — this runs from
    /// `render`, and taking the global mutably there queues a global-observer
    /// effect on every frame.
    fn scm_network_busy(&self, repo: &RepoKey, cx: &gpui::App) -> bool {
        cx.try_global::<crate::terminal::git_data::ScmData>()
            .is_some_and(|data| data.network_slots(repo.host, &repo.root) > 0)
    }

    /// How many repositories other than this one the panel could switch to.
    fn scm_other_repos(&self, current: &RepoKey) -> Option<usize> {
        let choices = self.scm_repo_choices();
        let count = choices.len().saturating_sub(1);
        (count > 0 && choices.contains(current)).then_some(count)
    }

    /// Read the local branch names, at most once per epoch.
    fn scm_load_branches(&mut self, repo: &RepoKey, cx: &mut Context<Self>) {
        let epoch = cx
            .default_global::<crate::terminal::git_data::ScmData>()
            .epoch(repo.host, &repo.root);
        if self
            .scm
            .branches
            .get(repo)
            .is_some_and(|(at, _)| *at == epoch)
            || self.scm.branches_loading.contains(repo)
        {
            return;
        }
        let Some(host) = crate::ui::host_registry::HostRegistry::get(cx, repo.host) else {
            return;
        };
        self.scm.branches_loading.insert(repo.clone());
        let root = repo.root.clone();
        let key = repo.clone();
        crate::ui::host_ops::HostOps::run(
            host,
            cx,
            move |h| {
                // `for-each-ref` rather than `branch`: no porcelain warnings,
                // no column layout, and one name per line whatever the config.
                tty7_core::core::git::git(
                    h,
                    &root,
                    &["for-each-ref", "--format=%(refname:short)", "refs/heads"],
                )
            },
            move |this, out, cx| {
                this.scm.branches_loading.remove(&key);
                let names = out
                    .unwrap_or_default()
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(str::to_string)
                    .collect();
                this.scm.branches.insert(key, (epoch, names));
                cx.notify();
            },
        );
    }

    fn scm_branch_menu(
        &self,
        repo: &RepoKey,
        status: &WorkingTreeStatus,
        cx: &mut Context<Self>,
    ) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static + use<>
    {
        let app = cx.entity().downgrade();
        let repo = repo.clone();
        let current = match &status.head {
            HeadState::Branch { name, .. } | HeadState::Unborn { branch: name } => name.clone(),
            HeadState::Detached { .. } => String::new(),
        };
        let branches = self
            .scm
            .branches
            .get(&repo)
            .map(|(_, names)| names.clone())
            .unwrap_or_default();
        let others = self.scm_repo_choices();

        move |menu, _window, _cx| {
            let mut menu = menu.min_w(px(200.));
            // Past a dozen the list stops being scannable, so it scrolls
            // rather than growing taller than the window.
            if branches.len() > BRANCHES_IN_MENU {
                menu = menu.scrollable(true).max_h(px(300.));
            }
            for name in &branches {
                let is_current = *name == current;
                menu = menu.item(
                    PopupMenuItem::new(name.clone())
                        .checked(is_current)
                        .disabled(is_current)
                        .on_click({
                            let app = app.clone();
                            let repo = repo.clone();
                            let name = name.clone();
                            move |_, window, cx| {
                                let _ = app.update(cx, |this, cx| {
                                    this.scm_op(
                                        repo.clone(),
                                        GitOp::CheckoutBranch { name: name.clone() },
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
                .item(PopupMenuItem::new(t(L10nKey::ScmCreateBranch)).on_click({
                    let app = app.clone();
                    move |_, window, cx| {
                        let _ = app.update(cx, |this, cx| this.scm_begin_create_branch(window, cx));
                    }
                }));
            for (label, intent) in [
                (L10nKey::ScmFetch, ScmIntent::Fetch),
                (L10nKey::ScmPull, ScmIntent::Pull),
                (L10nKey::ScmPush, ScmIntent::Push),
            ] {
                menu = menu.item(PopupMenuItem::new(t(label)).on_click({
                    let app = app.clone();
                    move |_, window, cx| {
                        let _ = app.update(cx, |this, cx| this.run_scm_action(intent, window, cx));
                    }
                }));
            }
            if others.len() > 1 {
                menu = menu
                    .separator()
                    .item(PopupMenuItem::label(t(L10nKey::ScmSwitchRepository)));
                for other in &others {
                    let label = other
                        .root
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| other.root.display().to_string());
                    menu = menu.item(PopupMenuItem::new(label).checked(*other == repo).on_click({
                        let app = app.clone();
                        let other = other.clone();
                        move |_, _window, cx| {
                            let _ = app.update(cx, |this, cx| {
                                this.scm.repo_override = Some(other.clone());
                                this.scm.override_tab = Some(this.active);
                                cx.notify();
                            });
                        }
                    }));
                }
            }
            menu
        }
    }

    /// Every repository the panel has looked at this session.
    ///
    /// Built from the roots it resolved rather than from the open tabs,
    /// because only a resolved root is safe to act on: the cheap per-tab cache
    /// knows a repository's *home*, which is a different directory inside a
    /// linked worktree, and running an operation in the wrong tree is worse
    /// than not offering the switch.
    fn scm_repo_choices(&self) -> Vec<RepoKey> {
        let mut out: Vec<RepoKey> = Vec::new();
        for ((host, _cwd), (_, root)) in &self.scm.roots {
            let Some(root) = root else { continue };
            let key = RepoKey {
                host: *host,
                root: root.clone(),
            };
            if !out.contains(&key) {
                out.push(key);
            }
        }
        out.sort_by(|a, b| a.root.cmp(&b.root));
        out
    }

    pub(crate) fn scm_begin_create_branch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder(t(L10nKey::ScmCreateBranch)));
        let handle = input.read(cx).focus_handle(cx);
        self.scm.new_branch = Some(input);
        window.focus(&handle, cx);
        cx.notify();
    }

    /// The inline "name your branch" row.
    ///
    /// A text input rather than a dialog: `window.prompt` can only offer
    /// buttons, and the project has no modal component to reach for. The file
    /// tree names new files the same way.
    fn scm_new_branch_row(&mut self, repo: &RepoKey, cx: &mut Context<Self>) -> Option<AnyElement> {
        let input = self.scm.new_branch.clone()?;
        let repo = repo.clone();
        Some(
            h_flex()
                .id("scm-new-branch")
                .flex_none()
                .items_center()
                // Every input row in the panel is 30px, and the field inside
                // it is the `.xsmall()` one that height was derived from.
                .h(px(30.))
                .px(px(CONTENT_INSET))
                .child(div().flex_1().min_w_0().child(Input::new(&input).xsmall()))
                .on_key_down(
                    cx.listener(move |this, ev: &gpui::KeyDownEvent, window, cx| {
                        match ev.keystroke.key.as_str() {
                            "escape" => {
                                this.scm.new_branch = None;
                                cx.notify();
                            }
                            "enter" => {
                                let Some(input) = this.scm.new_branch.take() else {
                                    return;
                                };
                                let name = input.read(cx).value().trim().to_string();
                                cx.notify();
                                if name.is_empty() {
                                    return;
                                }
                                this.scm_op(
                                    repo.clone(),
                                    GitOp::CreateBranch {
                                        name,
                                        start: None,
                                        checkout: true,
                                    },
                                    window,
                                    cx,
                                );
                            }
                            _ => {}
                        }
                    }),
                )
                .into_any_element(),
        )
    }

    /// The message box.
    ///
    /// `key_context` rather than a focus trap: `secondary-enter` is
    /// `ToggleFullscreen` at the window level, and the two coexist because
    /// gpui resolves a keystroke by walking outwards from the focused node —
    /// the narrow context wins while the box has focus, and the window keeps
    /// the chord everywhere else.
    ///
    /// A hairline and no fill, like every other framed thing in the panel.
    /// Focus recolours the same hairline to `theme.ring` rather than adding
    /// anything: a border that appears on focus would move the text by a pixel
    /// every time the box was clicked, because gpui measures border-box.
    fn scm_commit_box(
        &mut self,
        repo: &RepoKey,
        status: &WorkingTreeStatus,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let input = self.scm_commit_input(repo, status, window, cx);
        let focused = input.read(cx).focus_handle(cx).is_focused(window);
        let sf = cx.global::<crate::ui::presets::Surfaces>().sidebar;
        div()
            .key_context(COMMIT_KEY_CONTEXT)
            .flex_none()
            .px(px(CONTENT_INSET))
            .pt(px(6.))
            .child(
                div()
                    // `MSG_MIN_H` is `panel_search`'s height and the paddings
                    // are what get it there; see the constants. The maximum is
                    // a rail rather than the truth — in auto-grow mode the
                    // input sizes itself off its row count, so `MSG_ROWS_MAX`
                    // is what actually stops it, and this holds the shape if it
                    // ever measures itself differently.
                    .min_h(px(MSG_MIN_H))
                    .max_h(px(MSG_MAX_H))
                    .rounded(CARD_RADIUS)
                    // Half a rung, not a whole one. The hover step is what a
                    // row wears when the pointer is on it — a transient state —
                    // and the message box wears its fill all the time, so at
                    // full strength it read as the loudest thing on an idle
                    // panel. Focus then takes the whole rung, which keeps the
                    // two apart without a ring: the caret already says where
                    // the keystrokes go, and the fill only has to say which
                    // shape owns them.
                    .bg(gpui::rgb(match focused {
                        true => sf.hover,
                        false => field_fill(sf),
                    }))
                    .px(px(MSG_PAD_X))
                    .py(px(MSG_PAD_Y))
                    // `.xsmall()` is also what sets the box's height, because
                    // it is the size whose `input_py` is zero — see the
                    // `MSG_*` constants.
                    .child(Input::new(&input).appearance(false).xsmall()),
            )
            .into_any_element()
    }

    /// Hand the box the draft belonging to the repository on screen, and keep
    /// whatever is in it under the repository it was typed for.
    ///
    /// Per repository rather than per tab or per pane: a working tree has one
    /// pending message however many panes are looking at it.
    fn scm_commit_input(
        &mut self,
        repo: &RepoKey,
        status: &WorkingTreeStatus,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Entity<InputState> {
        let input = match self.scm.commit_input.clone() {
            Some(input) => input,
            None => {
                let input = cx.new(|cx| {
                    InputState::new(window, cx)
                        .multi_line(true)
                        .auto_grow(MSG_ROWS, MSG_ROWS_MAX)
                        .placeholder(t(L10nKey::ScmCommitPlaceholder))
                });
                self.scm.commit_input = Some(input.clone());
                input
            }
        };
        let text = input.read(cx).value().to_string();

        if self.scm.commit_repo.as_ref() != Some(repo) {
            if let Some(previous) = self.scm.commit_repo.take() {
                self.scm.drafts.insert(previous, text);
            }
            let next = match self.scm.drafts.get(repo) {
                Some(draft) => draft.clone(),
                // A merge or a cherry-pick leaves git's own message in
                // `.git/MERGE_MSG`; starting from blank would throw away the
                // conflict summary the user is about to want.
                None => status.prefilled_message.clone().unwrap_or_default(),
            };
            input.update(cx, |state, cx| state.set_value(next, window, cx));
            self.scm.commit_repo = Some(repo.clone());
            return input;
        }

        if self.scm_commit_landed(repo, status, &text) {
            input.update(cx, |state, cx| state.set_value("", window, cx));
            return input;
        }
        if self.scm.drafts.get(repo).map(String::as_str) != Some(text.as_str()) {
            self.scm.drafts.insert(repo.clone(), text);
        }
        input
    }

    /// Whether the commit we dispatched actually happened, and so whether the
    /// message may be thrown away.
    ///
    /// Clearing the box the moment `git commit` is *dispatched* would lose a
    /// carefully written message to a pre-commit hook that rejects it. HEAD
    /// moving is the one signal that says the message is now in the
    /// repository; an edit made in the meantime keeps it too.
    fn scm_commit_landed(
        &mut self,
        repo: &RepoKey,
        status: &WorkingTreeStatus,
        text: &str,
    ) -> bool {
        let Some((sent_repo, before, message)) = &self.scm.committing else {
            return false;
        };
        if sent_repo != repo || *before == status.head || message != text {
            return false;
        }
        self.scm.committing = None;
        self.scm.drafts.remove(repo);
        true
    }

    /// The line under the message box: what is staged on the left, what to do
    /// about it on the right.
    ///
    /// One split control, not a bar across the panel: the label button and the
    /// chevron share a single hairline frame with a 1px divider between them,
    /// and the pair is only as wide as the label. Right-aligned, with the
    /// count it acts on reading along the same line.
    ///
    /// Ghost inside the frame in both states, so the panel at rest holds no
    /// filled slab. The obvious alternative, gpui-component's `.primary()`,
    /// paints a grey one: `theme.primary` is `mix(foreground, background,
    /// 0.20)` in tty7, so a "primary" button is a mid-grey block sitting in a
    /// column of hairlines. What tells the two states apart is the ink —
    /// full-strength when there is something to commit, muted when there is
    /// not, which is where the panel sits most of the time.
    fn scm_commit_buttons(
        &self,
        repo: &RepoKey,
        status: &WorkingTreeStatus,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let plan = commit_plan(status, self.scm.amend, self.scm.draft(repo));
        let repo_for_button = repo.clone();
        let live = plan.enabled;
        let staged = status.staged().count();
        let theme = cx.theme();
        let (muted, fg) = (theme.muted_foreground, theme.foreground);
        let sf = cx.global::<crate::ui::presets::Surfaces>().sidebar;
        h_flex()
            .flex_none()
            .items_center()
            .gap(px(8.))
            .px(px(CONTENT_INSET))
            .pt(px(6.))
            .pb(px(8.))
            // The reading the button acts on, in the row it acts from. It
            // gives way first: a long count is still a count, and the control
            // beside it is the thing that has to keep its shape.
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(11.))
                    .text_color(muted)
                    .child(t_plural(L10nKey::ScmStagedFileCount, staged, &[])),
            )
            .child(
                // Filled, not outlined, and for the same reason the message box
                // above it is: nothing else in this panel wears a border, and a
                // hairline box sitting on the panel's own fill reads as raised
                // — a shadow nobody drew.
                //
                // The frame owns the fill, the rounding *and* the hover, so the
                // control lights as one shape. Letting each half light itself
                // is what a split button conventionally does, but this one has
                // no outline around either half and a seam only a pixel wide:
                // half a lit pill reads as a paint bug rather than as "this is
                // the part you are on". The halves still carry
                // `segment_corners` so their own hit shapes stay inside the
                // frame's radius — `overflow_hidden` would do that too, and
                // would take the chevron's popup menu with it.
                h_flex()
                    .flex_none()
                    .items_center()
                    .h(px(COMMIT_H))
                    .rounded(CARD_RADIUS)
                    .bg(gpui::rgb(field_fill(sf)))
                    .hover(|s| s.bg(gpui::rgb(sf.hover)))
                    .child(
                        // `xsmall` for the same reason the branch trigger is
                        // `xsmall`: it is the token that sets a `Button`'s
                        // label to 12px, the panel's own step. The padding and
                        // the height are set below, so the size token is doing
                        // nothing here but choosing the type.
                        Button::new("scm-commit")
                            // A custom variant, not `.ghost()`: the frame is
                            // already painted at the ramp's hover rung, and a
                            // ghost hovers to a colour close enough to it that
                            // the pointer would get no answer. This walks the
                            // same two rungs the file rows walk.
                            .custom(commit_half(cx))
                            .xsmall()
                            .label(t(plan.label))
                            .h_full()
                            .px(px(10.))
                            .rounded_corners(segment_corners(0, 2, CARD_RADIUS, HAIRLINE))
                            // The ink is named here rather than left to the
                            // variant, so the disabled half greys out: it lands
                            // over the variant's own paint because
                            // `refine_style` runs after everything the variant
                            // does.
                            .text_color(if live { fg } else { muted })
                            .disabled(!live)
                            .when(!live, |b| b.tooltip(t(plan.reason)))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.scm_commit(
                                    repo_for_button.clone(),
                                    this.scm.amend,
                                    None,
                                    window,
                                    cx,
                                );
                            })),
                    )
                    // The seam between the halves is the panel showing through
                    // the fill, not a line drawn on top of it — a rule would be
                    // the one stroke left on the panel. A div rather than
                    // `border_l_1` on the chevron, because a ghost `Button`
                    // repaints its border colour transparent in every state it
                    // has and the seam would vanish under the pointer.
                    .child(
                        div()
                            .flex_none()
                            .w(HAIRLINE)
                            .h_full()
                            .bg(gpui::rgb(sf.base)),
                    )
                    .child(self.scm_commit_menu(repo, cx)),
            )
            .into_any_element()
    }

    /// The chevron half of the split control.
    ///
    /// Never disabled, whatever the button beside it is doing: stash and amend
    /// still mean something with nothing staged, and this is the only way to
    /// reach them.
    fn scm_commit_menu(&self, repo: &RepoKey, cx: &mut Context<Self>) -> AnyElement {
        let amend = self.scm.amend;
        Button::new("scm-commit-menu")
            .custom(commit_half(cx))
            .icon(Icon::new(IconName::ChevronDown))
            // `Button` sizes an icon off its own `Size`, so the glyph is asked
            // for by way of the size that produces it — the same conversion
            // `chrome_tile_sized` does.
            .with_size(px(COMMIT_GLYPH / crate::ui::tab_strip::BUTTON_ICON_SCALE))
            .w(px(COMMIT_CHEVRON_W))
            .h_full()
            .rounded_corners(segment_corners(1, 2, CARD_RADIUS, HAIRLINE))
            .dropdown_menu_with_anchor(gpui::Anchor::TopRight, {
                let app = cx.entity().downgrade();
                let repo = repo.clone();
                move |menu, _window, _cx| {
                    let mut menu = menu.min_w(px(190.));
                    for (label, intent) in [
                        (L10nKey::ScmCommitButton, ScmIntent::Commit),
                        (L10nKey::ScmCommitAndPush, ScmIntent::CommitAndPush),
                        (L10nKey::ScmCommitAndSync, ScmIntent::CommitAndSync),
                    ] {
                        menu = menu.item(PopupMenuItem::new(t(label)).on_click({
                            let app = app.clone();
                            move |_, window, cx| {
                                let _ = app
                                    .update(cx, |this, cx| this.run_scm_action(intent, window, cx));
                            }
                        }));
                    }
                    menu = menu.separator().item(
                        // A menu item rather than a checkbox row: the panel is
                        // 260px wide, and the armed state already shows up in the
                        // button's label and in the chip on the branch row.
                        PopupMenuItem::new(t(L10nKey::ScmAmendLastCommit))
                            .checked(amend)
                            .on_click({
                                let app = app.clone();
                                move |_, _window, cx| {
                                    let _ = app.update(cx, |this, cx| {
                                        this.scm.amend = !this.scm.amend;
                                        cx.notify();
                                    });
                                }
                            }),
                    );
                    menu.separator()
                        .item(PopupMenuItem::new(t(L10nKey::ScmStashAll)).on_click({
                            let app = app.clone();
                            let repo = repo.clone();
                            move |_, window, cx| {
                                let _ = app.update(cx, |this, cx| {
                                    this.scm_stash_all(repo.clone(), window, cx)
                                });
                            }
                        }))
                }
            })
            .into_any_element()
    }

    /// Title over a scrolling body, with the panel's own scroll handle.
    ///
    /// Not `panel_scroll`: that one owns `right_panel.scroll`, and the rows
    /// that land between the title and the list in later steps have to stay
    /// pinned while the list moves under them.
    fn scm_shell(&self, title: AnyElement, body: AnyElement) -> AnyElement {
        self.scm_shell_with(title, Vec::new(), body)
    }

    /// `pinned` rows sit between the title and the list and do not scroll:
    /// the message box has to stay reachable however far down the files go.
    fn scm_shell_with(
        &self,
        title: AnyElement,
        pinned: Vec<AnyElement>,
        body: AnyElement,
    ) -> AnyElement {
        self.scm_shell_full(title, pinned, body, None)
    }

    /// …and `footer` is the history section, which scrolls on its own.
    ///
    /// The four bands sit flush on the panel with nothing between them — no
    /// gutter, no fill, no rounded blocks. What separates them is the same
    /// thing that separates every other stack of rows in the right panel: the
    /// rows' own insets and the pause between them. Each band keeps its own
    /// `CONTENT_INSET`, so the panel's text column stays one column from the
    /// title to the last commit in the history.
    fn scm_shell_full(
        &self,
        title: AnyElement,
        pinned: Vec<AnyElement>,
        body: AnyElement,
        footer: Option<AnyElement>,
    ) -> AnyElement {
        let scroller = div()
            .id("panel-scm-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.scm.scroll)
            .child(body);
        v_flex()
            .flex_1()
            .min_h_0()
            .child(title)
            .children(pinned)
            .child(crate::ui::scrollbar::with_vertical_scrollbar(
                "panel-scm-scrollbar",
                scroller,
                &self.scm.scroll,
            ))
            .children(footer)
            .into_any_element()
    }

    /// The host and directory the panel is looking at.
    fn scm_pane_target(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<(SharedHost, PathBuf)> {
        let leaf = self.tabs.get(self.active)?.detail_pane(window, cx)?;
        let view = leaf.read(cx);
        let cwd = view.effective_host_cwd()?;
        Some((view.host(cx)?, cwd))
    }

    /// Turn the pane's directory into the repository root every write has to
    /// run from.
    ///
    /// Pathspecs out of `status --porcelain=v2` are relative to the root, so
    /// running `git add` from a subdirectory would name the wrong files. The
    /// root is also the cache key, which is what lets two panes in two
    /// subdirectories of one repository share a single status.
    ///
    /// Resolved with its own `rev-parse` rather than borrowed from the cheap
    /// per-tab cache. That cache holds a repository's *home*, which is a
    /// different directory inside a linked worktree, and it is only filled in
    /// for panes whose shell reports a cwd — a pane without shell integration
    /// would leave the panel loading forever.
    fn scm_repo_root(
        &mut self,
        host: &SharedHost,
        cwd: &Path,
        cx: &mut Context<Self>,
    ) -> RepoLookup {
        let id = host.id();
        let key = (id, cwd.to_path_buf());
        match self.scm.roots.get(&key) {
            Some((_, Some(root))) => return RepoLookup::Root(root.clone()),
            // "Not a repository" is re-asked now and then, because `git init`
            // in the pane below has to start showing up without a restart.
            Some((at, None)) if at.elapsed() < NOT_A_REPO_RETRY => return RepoLookup::NotARepo,
            _ => {}
        }
        if !self.scm.root_lookups.insert(key.clone()) {
            return RepoLookup::Pending;
        }
        let dir = cwd.to_path_buf();
        crate::ui::host_ops::HostOps::run(
            host.clone(),
            cx,
            move |h| {
                // Asked separately from the status probe, and asked first: the
                // answer is what everything else is keyed by, it is the same
                // for every pane in the tree, and it is a ref lookup rather
                // than a walk of the working tree.
                tty7_core::core::git::git(
                    h,
                    &dir,
                    &["rev-parse", "--path-format=absolute", "--show-toplevel"],
                )
            },
            move |this, out, cx| {
                this.scm.root_lookups.remove(&key);
                let root = out
                    .as_deref()
                    .and_then(|s| s.lines().next())
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(PathBuf::from);
                this.scm.roots.insert(key, (Instant::now(), root));
                cx.notify();
            },
        );
        RepoLookup::Pending
    }

    /// `scm_refresh` with a floor under how often a fruitless probe repeats.
    fn scm_probe(&mut self, host: &SharedHost, root: &Path, cx: &mut Context<Self>) {
        let key = (host.id(), root.to_path_buf());
        if status_of(cx, host.id(), root).is_none() {
            let now = Instant::now();
            match self.scm.probe_attempt.get(&key) {
                Some(at) if now.duration_since(*at) < PROBE_RETRY => return,
                _ => {
                    self.scm.probe_attempt.insert(key, now);
                }
            }
        }
        self.scm_refresh(host.clone(), root.to_path_buf(), cx);
    }

    /// Read the status and record which one this frame drew, so the watcher
    /// below can tell a real change from its own noise.
    fn scm_seen_status(
        &mut self,
        host: HostId,
        root: &Path,
        cx: &mut Context<Self>,
    ) -> Option<Arc<WorkingTreeStatus>> {
        let status = status_of(cx, host, root);
        self.scm.seen = Some((
            (host, root.to_path_buf()),
            status.as_ref().map_or(0, |s| Arc::as_ptr(s) as usize),
        ));
        status
    }

    /// Re-render when a probe lands.
    ///
    /// The subscription has to compare before it notifies. `scm_refresh`
    /// reaches for `ScmData` through `default_global`, which fires the global
    /// observers whether or not anything changed — and it is called from
    /// `render`. An unconditional `cx.notify()` here would therefore ask for a
    /// frame from inside a frame, forever.
    fn scm_watch_status(&mut self, cx: &mut Context<Self>) {
        if self.scm.watch.is_some() {
            return;
        }
        self.scm.watch = Some(cx.observe_global::<crate::terminal::git_data::ScmData>(
            |this, cx| {
                let Some((key, seen)) = this.scm.seen.clone() else {
                    return;
                };
                let now = status_of(cx, key.0, &key.1).map_or(0, |s| Arc::as_ptr(&s) as usize);
                if now != seen {
                    this.scm.seen = Some((key, now));
                    cx.notify();
                }
            },
        ));
    }

    fn scm_groups(
        &mut self,
        repo: &RepoKey,
        status: &Arc<WorkingTreeStatus>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut list = v_flex().px(px(CONTENT_INSET - ROW_INSET)).py(px(2.));
        for group in ScmGroup::ORDER {
            let entries: Vec<&StatusEntry> = status
                .entries
                .iter()
                .filter(|e| in_group(e, group))
                .collect();
            if entries.is_empty() {
                continue;
            }
            let collapsed = self.scm.group_collapsed(group, entries.len());
            list = list.child(self.scm_group_header(repo, group, &entries, collapsed, cx));
            if collapsed {
                continue;
            }
            let shown = entries.len().min(MAX_RENDERED_FILES);
            for entry in entries.iter().take(shown) {
                list = list.child(self.scm_file_row(repo, group, entry, cx));
            }
            if entries.len() > shown {
                list = list.child(self.scm_note(
                    t_plural(L10nKey::PanelMoreChangedFiles, entries.len() - shown, &[]),
                    cx,
                ));
            }
        }
        if status.truncated {
            list = list.child(self.scm_note(
                t_fmt(
                    L10nKey::ScmTooManyChanges,
                    &[
                        ("shown", &status.entries.len().to_string()),
                        ("total", &status.total_entries.to_string()),
                    ],
                ),
                cx,
            ));
        }
        list.into_any_element()
    }

    /// "…and 40 more", and the note about a status git had to truncate.
    ///
    /// Secondary: it is prose about the list rather than a row of it, and the
    /// one thing it must not do is read as another file.
    fn scm_note(&self, text: String, cx: &mut Context<Self>) -> AnyElement {
        div()
            .px(px(ROW_INSET))
            .py(px(3.))
            .text_size(px(11.))
            .text_color(cx.theme().muted_foreground.opacity(0.75))
            .child(text)
            .into_any_element()
    }

    fn scm_group_header(
        &self,
        repo: &RepoKey,
        group: ScmGroup,
        entries: &[&StatusEntry],
        collapsed: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let count = entries.len();
        let sf = cx.global::<crate::ui::presets::Surfaces>().sidebar;
        let mono = cx.theme().mono_font_family.clone();
        let id = SharedString::from(format!("scm-group-{group:?}"));
        let actions = self.scm_group_actions(&id, repo, group, entries, sf.hover, cx);
        h_flex()
            .id(id.clone())
            .group(id)
            .relative()
            .items_center()
            .gap(px(8.))
            .h(px(ROW_H))
            .px(px(ROW_INSET))
            .rounded(px(5.))
            .cursor_pointer()
            .hover(|s| s.bg(gpui::rgb(sf.hover)))
            .on_click(cx.listener(move |this, _, _window, cx| {
                this.scm_toggle_group(group, count, cx);
            }))
            // The chevron's box is exactly the width of `git_badge`, so this
            // column and the status letters below it are one straight line.
            .child(
                div()
                    .flex_none()
                    .w(px(BADGE_W))
                    .flex()
                    .justify_center()
                    .text_color(cx.theme().muted_foreground)
                    .child(
                        Icon::new(if collapsed {
                            IconName::ChevronRight
                        } else {
                            IconName::ChevronDown
                        })
                        .size(px(11.)),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(10.5))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(cx.theme().muted_foreground)
                    .child(t(group_label(group)).to_uppercase()),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(px(11.))
                    .font_family(mono)
                    .text_color(cx.theme().muted_foreground.opacity(0.75))
                    .child(count.to_string()),
            )
            .child(actions)
            .into_any_element()
    }

    fn scm_file_row(
        &self,
        repo: &RepoKey,
        group: ScmGroup,
        entry: &StatusEntry,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let sf = cx.global::<crate::ui::presets::Surfaces>().sidebar;
        let mono = cx.theme().mono_font_family.clone();
        let path = entry.path.as_str().to_string();
        let (name, dir) = split_display_path(&path);
        let (letter, deco) = row_status(entry, group);
        let source = group_diff_source(group);
        let selected =
            self.diff_overlay_focus(repo.host, &repo.root, &source) == Some(path.as_str());
        let id = SharedString::from(format!("scm-row-{group:?}-{path}"));
        let actions = self.scm_row_actions(
            &id,
            repo,
            group,
            entry,
            if selected { sf.selected } else { sf.hover },
            cx,
        );

        h_flex()
            .id(id.clone())
            .group(id)
            .relative()
            .items_center()
            .gap(px(8.))
            .h(px(ROW_H))
            .px(px(ROW_INSET))
            .py(px(3.))
            .rounded(px(5.))
            .cursor_pointer()
            .hover(|s| s.bg(gpui::rgb(sf.hover)))
            .when(selected, |s| s.bg(gpui::rgb(sf.selected)))
            .on_click({
                let repo = repo.clone();
                let path = path.clone();
                cx.listener(move |this, _, window, cx| {
                    this.open_diff_overlay(
                        repo.host,
                        repo.root.clone(),
                        source.clone(),
                        Some(path.clone()),
                        window,
                        cx,
                    );
                })
            })
            .context_menu({
                let app = cx.entity().downgrade();
                let repo = repo.clone();
                let entry = entry.clone();
                move |menu, _window, cx| {
                    Self::scm_row_context_menu(menu, &app, &repo, group, &entry, cx)
                }
            })
            .child(git_badge(letter, status_color(deco, cx), &mono))
            // Mono, because a path is a token you compare character by
            // character.
            //
            // `scm/detail.rs` draws the changed-file rows of a commit and
            // spells this number and the directory's out by hand so the two
            // lists stay pixel-identical. Moving one means moving the other.
            .child(
                div()
                    .flex_none()
                    .text_size(px(12.))
                    .font_family(mono.clone())
                    .text_color(if deco == DecoStatus::Deleted {
                        cx.theme().muted_foreground
                    } else {
                        cx.theme().foreground
                    })
                    .when(deco == DecoStatus::Deleted, |s| s.line_through())
                    .child(name.to_string()),
            )
            // The directory gives way first: which file it is matters more
            // than where it lives, and the name is already the shorter half.
            .when(!dir.is_empty(), |this| {
                this.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(11.))
                        .text_color(cx.theme().muted_foreground.opacity(0.75))
                        .child(dir.to_string()),
                )
            })
            .child(actions)
            .into_any_element()
    }

    /// The buttons that appear over a hovered row.
    fn scm_row_actions(
        &self,
        row: &SharedString,
        repo: &RepoKey,
        group: ScmGroup,
        entry: &StatusEntry,
        backing: u32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // A path git cannot be given is a path nothing can be done to. The
        // row stays readable and the buttons say why they are dead.
        let writable = entry.path.pathspec().is_some();
        let path = entry.path.as_str();
        let mut actions = action_strip(row, backing);

        for &(verb, ref icon) in row_verbs(group) {
            let id = SharedString::from(format!("scm-{verb:?}-{group:?}-{path}"));
            let repo = repo.clone();
            let entry = entry.clone();
            actions = actions.child(
                self.scm_tile(id, icon.clone(), verb_tooltip(verb), writable, cx)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.scm_row_verb(verb, &repo, group, &entry, window, cx);
                    })),
            );
        }
        actions.into_any_element()
    }

    fn scm_group_actions(
        &self,
        row: &SharedString,
        repo: &RepoKey,
        group: ScmGroup,
        entries: &[&StatusEntry],
        backing: u32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let paths = writable_paths(entries);
        let mut actions = action_strip(row, backing);

        for &(verb, ref icon) in group_verbs(group) {
            let id = SharedString::from(format!("scm-all-{verb:?}-{group:?}"));
            let repo = repo.clone();
            let paths = paths.clone();
            actions = actions.child(
                self.scm_tile(
                    id,
                    icon.clone(),
                    verb_all_tooltip(verb),
                    !paths.is_empty(),
                    cx,
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    let Some(op) = verb_op(verb, group, paths.clone()) else {
                        return;
                    };
                    this.scm_op(repo.clone(), op, window, cx);
                })),
            );
        }
        actions.into_any_element()
    }

    /// A [`TILE_SIZE_XS`] tile. Smaller than `TILE_SIZE_SM`, because three of
    /// those on a row would eat 72 of the 236px a file name has to live in.
    fn scm_tile(
        &self,
        id: SharedString,
        icon: IconName,
        tooltip: &'static str,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> Button {
        crate::ui::tab_strip::chrome_tile_sized(
            Button::new(id).icon(Icon::new(icon.clone())),
            TILE_SIZE_XS,
            TILE_GLYPH_XS,
            false,
            cx,
        )
        .rounded(px(4.))
        .disabled(!enabled)
        .tooltip(if enabled {
            tooltip
        } else {
            t(L10nKey::ScmUnrepresentablePath)
        })
    }

    fn scm_row_verb(
        &mut self,
        verb: RowVerb,
        repo: &RepoKey,
        group: ScmGroup,
        entry: &StatusEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if verb == RowVerb::OpenConflict {
            self.open_diff_overlay(
                repo.host,
                repo.root.clone(),
                group_diff_source(group),
                Some(entry.path.as_str().to_string()),
                window,
                cx,
            );
            return;
        }
        let Some(op) = verb_op(verb, group, vec![entry.path.clone()]) else {
            return;
        };
        self.scm_op(repo.clone(), op, window, cx);
    }

    fn scm_row_context_menu(
        menu: PopupMenu,
        app: &gpui::WeakEntity<Self>,
        repo: &RepoKey,
        group: ScmGroup,
        entry: &StatusEntry,
        cx: &gpui::App,
    ) -> PopupMenu {
        let danger = cx.theme().danger;
        let rel = entry.path.as_str().to_string();
        let absolute = repo.root.join(&rel);
        let source = group_diff_source(group);
        let staged = group == ScmGroup::Staged;

        let mut menu = menu
            .min_w(px(200.))
            .item(
                PopupMenuItem::new(t(L10nKey::FileTreeContextOpen)).on_click({
                    let app = app.clone();
                    let absolute = absolute.clone();
                    move |_, window, cx| {
                        let _ = app.update(cx, |this, cx| {
                            this.open_file_in_editor(&absolute, window, cx)
                        });
                    }
                }),
            )
            .item(PopupMenuItem::new(t(L10nKey::ScmOpenChanges)).on_click({
                let app = app.clone();
                let repo = repo.clone();
                let rel = rel.clone();
                move |_, window, cx| {
                    let _ = app.update(cx, |this, cx| {
                        this.open_diff_overlay(
                            repo.host,
                            repo.root.clone(),
                            source.clone(),
                            Some(rel.clone()),
                            window,
                            cx,
                        );
                    });
                }
            }))
            .separator()
            .item(
                PopupMenuItem::new(if staged {
                    t(L10nKey::ScmUnstage)
                } else {
                    t(L10nKey::ScmStage)
                })
                .on_click({
                    let app = app.clone();
                    let repo = repo.clone();
                    let paths = vec![entry.path.clone()];
                    move |_, window, cx| {
                        let op = if staged {
                            GitOp::Unstage {
                                paths: paths.clone(),
                            }
                        } else {
                            GitOp::Stage {
                                paths: paths.clone(),
                            }
                        };
                        let _ =
                            app.update(cx, |this, cx| this.scm_op(repo.clone(), op, window, cx));
                    }
                }),
            )
            .separator()
            .item(
                PopupMenuItem::new(t(L10nKey::FileTreeContextCopyPath)).on_click({
                    let absolute = absolute.clone();
                    move |_, _window, cx| {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                            absolute.display().to_string(),
                        ));
                    }
                }),
            );

        // Revealing a path only means anything on the machine the window is
        // running on; a remote repository's paths are not this filesystem's.
        if repo.host == HostId::LOCAL {
            menu = menu.item(
                PopupMenuItem::new(crate::ui::right_panel::reveal_label()).on_click({
                    let absolute = absolute.clone();
                    move |_, _window, cx| cx.reveal_path(&absolute)
                }),
            );
        }

        if let Some(op) = verb_op(RowVerb::Discard, group, vec![entry.path.clone()]) {
            menu = menu.separator().item(
                PopupMenuItem::element(move |_window, _cx| {
                    div().text_color(danger).child(t(L10nKey::ScmDiscard))
                })
                .on_click({
                    let app = app.clone();
                    let repo = repo.clone();
                    move |_, window, cx| {
                        let op = op.clone();
                        let _ =
                            app.update(cx, |this, cx| this.scm_op(repo.clone(), op, window, cx));
                    }
                }),
            );
        }
        menu
    }

    /// What `app.rs`'s `GitStatusCache` observer calls when the cheap
    /// per-tab probe lands.
    ///
    /// That probe runs at every command boundary, which makes it the best
    /// signal there is that the working tree moved — far better than a timer.
    /// The comparison in front of the bump is what keeps it from turning every
    /// notification (including the one the probe's *start* fires) into another
    /// `git status`.
    pub(crate) fn right_panel_refresh_changes(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.scm.repo.clone() else {
            return;
        };
        let Some(seen) = cx
            .try_global::<crate::terminal::git_status::GitStatusCache>()
            .and_then(|cache| cache.status_for(repo.host, &repo.root))
        else {
            return;
        };
        if self.scm.last_tab_status.as_ref() == Some(&seen) {
            return;
        }
        self.scm.last_tab_status = Some(seen);
        self.scm_invalidate(repo.host, &repo.root, cx);
        cx.notify();
    }

    fn scm_toggle_group(&mut self, group: ScmGroup, count: usize, cx: &mut Context<Self>) {
        let collapsed = self.scm.group_collapsed(group, count);
        self.scm.set_group_collapsed(group, !collapsed);
        cx.notify();
    }
}

/// The strip the row and group action buttons live in, revealed by hovering
/// `row`.
///
/// Absolutely positioned and opaque, so it covers the tail of the directory
/// rather than pushing it aside: hovering a row must not move a single pixel
/// of it, or the list crawls under the pointer.
///
/// It stops the mouse-down by hand instead of calling `occlude()`, which is
/// the obvious way to keep a click off the row underneath and was what made
/// the buttons vanish the moment the pointer reached them. `occlude()` is a
/// *hitbox* behaviour, and gpui inserts hitboxes in prepaint, which never
/// looks at `visibility` — so the strip blocked the mouse even while it was
/// invisible. Blocking cuts the hit test short at the blocking hitbox, and the
/// row's hitbox is behind this one because a parent prepaints before its
/// children; `group_hover` is nothing more than "is the group's hitbox
/// hovered", so the row stopped counting as hovered and the strip hid itself
/// — background, buttons and all — with the pointer sitting right on it. The
/// buttons' own hitboxes came from prepaint and outlived the paint, so they
/// went on answering tooltips for glyphs that were no longer drawn.
///
/// Stopping propagation buys the same "this click is ours, not the row's"
/// without lying to the hit test: children register their handlers after this
/// one and gpui bubbles back to front, so a button still gets its click first.
fn action_strip(row: &SharedString, backing: u32) -> gpui::Div {
    h_flex()
        .absolute()
        .right(px(ROW_INSET))
        .top_0()
        .bottom_0()
        .items_center()
        .gap(px(1.))
        .bg(gpui::rgb(backing))
        .invisible()
        .group_hover(row.clone(), |s| s.visible())
        .on_any_mouse_down(|_, _, cx| cx.stop_propagation())
}

/// Which sections an entry shows up in.
///
/// A file can be staged and unstaged at once (`XY == "MM"`), and then it
/// belongs in both — the same thing VS Code shows, and the honest reading of
/// what a commit right now would contain.
pub(crate) fn in_group(entry: &StatusEntry, group: ScmGroup) -> bool {
    match group {
        ScmGroup::Merge => entry.is_conflicted(),
        ScmGroup::Untracked => entry.is_untracked(),
        ScmGroup::Staged => entry.is_staged(),
        ScmGroup::Changes => entry.is_unstaged() && !entry.is_untracked(),
    }
}

/// The letter and colour a row wears, decided by the half of `XY` its group
/// is about — a file staged as added and then modified is `A` under Staged
/// and `M` under Changes, which is what `git status` itself says.
pub(crate) fn row_status(entry: &StatusEntry, group: ScmGroup) -> (&'static str, DecoStatus) {
    let deco = match group {
        ScmGroup::Merge => DecoStatus::Conflict,
        ScmGroup::Untracked => DecoStatus::Untracked,
        ScmGroup::Staged => code_deco(entry.index),
        ScmGroup::Changes => code_deco(entry.worktree),
    };
    let letter = match group {
        // The two groups whose letter is fixed use the shared glyph table, so
        // a conflict is `U` here and in the file tree alike.
        ScmGroup::Merge | ScmGroup::Untracked => status_glyph(deco),
        ScmGroup::Staged => letter_of(entry.index),
        ScmGroup::Changes => letter_of(entry.worktree),
    };
    (letter, deco)
}

/// `ChangeCode::letter` returns a `char`; rows want a `&'static str` so the
/// badge never allocates. `T` and `C` keep their own letters rather than being
/// folded into `M` and `R` — git shows them, and they mean different things.
fn letter_of(code: ChangeCode) -> &'static str {
    match code {
        ChangeCode::None => " ",
        ChangeCode::Modified => "M",
        ChangeCode::TypeChanged => "T",
        ChangeCode::Added => "A",
        ChangeCode::Deleted => "D",
        ChangeCode::Renamed => "R",
        ChangeCode::Copied => "C",
        ChangeCode::Unmerged => "U",
    }
}

fn code_deco(code: ChangeCode) -> DecoStatus {
    match code {
        ChangeCode::Deleted => DecoStatus::Deleted,
        ChangeCode::Added => DecoStatus::Added,
        ChangeCode::Renamed | ChangeCode::Copied => DecoStatus::Renamed,
        ChangeCode::Unmerged => DecoStatus::Conflict,
        _ => DecoStatus::Modified,
    }
}

/// Which patch a row's click opens.
///
/// Staged rows show `git diff --cached`; everything else shows the working
/// tree. Getting this wrong is not cosmetic — the file name would be right and
/// the hunks underneath it would be someone else's.
pub(crate) fn group_diff_source(group: ScmGroup) -> DiffSource {
    match group {
        ScmGroup::Staged => DiffSource::Staged,
        ScmGroup::Merge | ScmGroup::Changes | ScmGroup::Untracked => DiffSource::Worktree,
    }
}

fn group_label(group: ScmGroup) -> L10nKey {
    match group {
        ScmGroup::Merge => L10nKey::ScmGroupMerge,
        ScmGroup::Staged => L10nKey::ScmGroupStaged,
        ScmGroup::Changes => L10nKey::ScmGroupChanges,
        ScmGroup::Untracked => L10nKey::ScmGroupUntracked,
    }
}

/// Whether a group nobody has touched starts folded.
pub(crate) fn starts_collapsed(group: ScmGroup, count: usize) -> bool {
    group == ScmGroup::Untracked && count > UNTRACKED_AUTO_COLLAPSE
}

/// What the branch row says where the branch name goes.
pub(crate) fn head_label(head: &HeadState) -> String {
    match head {
        // A detached HEAD has no name, so it wears its sha — shortened to the
        // seven characters git itself abbreviates to.
        HeadState::Detached { oid } => oid.chars().take(7).collect(),
        _ => head.label(),
    }
}

/// The `↑2 ↓0` reading, or nothing.
///
/// A branch that is level with its upstream says nothing at all: the quiet
/// state is the common one, and a token that is always there stops being read.
/// A branch with no upstream offers to publish instead.
///
/// Once there *is* something to say, both halves are said, zero included. The
/// pair is one reading of one distance, and dropping the empty half makes it
/// change shape as the numbers move — `↑2` becoming `↑2 ↓1` on a fetch is a
/// different-looking thing in the corner of the eye, where `↑2 ↓0` becoming
/// `↑2 ↓1` is the same thing with a digit changed.
pub(crate) fn tracking_chip(
    upstream: Option<&str>,
    ahead_behind: Option<(u32, u32)>,
) -> Option<String> {
    if upstream.is_none() {
        return Some(t(L10nKey::ScmPublishBranch).to_string());
    }
    match ahead_behind? {
        (0, 0) => None,
        (ahead, behind) => Some(format!("↑{ahead} ↓{behind}")),
    }
}

/// A quiet token on the branch row: the tracking distance, or the count of
/// other repositories.
///
/// Not `info_chip`. A chip's fill is what earns it the right to shout, and the
/// only two things on this row worth shouting are a state you could forget you
/// are in — detached, mid-rebase, armed to amend — and they already wear the
/// warning tint. `↑2 ↓0` is a reading, so it reads as text; the row's own 6px
/// gap is enough to keep it off the branch name without a box around it.
///
/// The same quiet mono step the panel's badges and chips are set on, minus the
/// fill, so a note and a chip still line up as one row of tokens.
/// The fill under the commit area's two shapes — the message box and the split
/// button's frame.
///
/// Half of the ramp's first rung, not the rung itself. `hover` is what a row
/// wears while the pointer is on it, which is a state that lasts a moment;
/// these two wear their fill permanently, and at full strength they were the
/// loudest thing on an idle panel. Both read from here so they cannot drift
/// apart: the commit area is one block, and two greys a shade off each other
/// look like a mistake rather than a hierarchy.
fn field_fill(sf: crate::ui::presets::Surface) -> u32 {
    crate::ui::presets::mix(sf.base, sf.hover, 0.5)
}

/// One half of the split commit control: paints nothing, ever.
///
/// Every state is transparent so the frame around both halves is the only
/// thing that fills, and the control lights as one shape rather than one end.
/// Two of those states are worth naming, because a stock variant gets them
/// wrong here in opposite directions:
///
/// * `.ghost()` hovers to `sidebar_accent`, which sits close enough to the
///   frame's own fill that the half would light almost invisibly — the worst of
///   both, a highlight you can see is uneven but cannot read.
/// * gpui-component resolves a `Custom` variant's *selected* paint from its
///   `active` slot, and a dropdown marks its trigger selected for as long as
///   the menu is open. Anything but transparent there parks a block on the
///   chevron for the whole time the user is reading the menu.
fn commit_half(cx: &gpui::App) -> ButtonCustomVariant {
    let clear = gpui::transparent_black();
    ButtonCustomVariant::new(cx)
        .color(clear)
        .foreground(cx.theme().foreground)
        .hover(clear)
        .active(clear)
}

fn branch_note(text: &str, ink: gpui::Hsla, mono: &SharedString) -> AnyElement {
    div()
        .flex_none()
        .text_size(px(10.5))
        .font_family(mono.clone())
        .text_color(ink)
        .child(text.to_string())
        .into_any_element()
}

/// Which sequencer operation is parked in the repository.
///
/// `RebaseInteractive` reads as "rebasing" on purpose: modern git writes
/// `rebase-merge/interactive` for every rebase, so the distinction the variant
/// name suggests is not one the repository on disk can actually make — and
/// `git status` does not draw it either.
pub(crate) fn operation_label(op: RepoOperation) -> L10nKey {
    match op {
        RepoOperation::Merge => L10nKey::ScmOpMerge,
        RepoOperation::Rebase | RepoOperation::RebaseInteractive => L10nKey::ScmOpRebase,
        RepoOperation::CherryPick => L10nKey::ScmOpCherryPick,
        RepoOperation::Revert => L10nKey::ScmOpRevert,
        RepoOperation::Bisect => L10nKey::ScmOpBisect,
        RepoOperation::Am => L10nKey::ScmOpAm,
    }
}

/// What the commit button says, and whether it can be pressed at all.
pub(crate) struct CommitPlan {
    pub(crate) label: L10nKey,
    pub(crate) enabled: bool,
    /// Why the button is disabled, when it is. "Nothing to commit" and "write
    /// a message" call for opposite actions, and one tooltip for both sends
    /// the user staging files they already staged.
    pub(crate) reason: L10nKey,
}

/// Decide both from the state of the index.
///
/// "Commit All" rather than a silent `-a`: with nothing staged, `git commit`
/// would commit nothing, and the honest thing is to say on the button that
/// every tracked change is about to go in. An armed amend needs no message —
/// `--no-edit` keeps the one that is already on HEAD.
pub(crate) fn commit_plan(status: &WorkingTreeStatus, amend: bool, message: &str) -> CommitPlan {
    let staged = status.staged().next().is_some();
    let tracked_edits = status.unstaged().next().is_some();
    let label = if amend {
        L10nKey::ScmCommitAmendButton
    } else if staged {
        L10nKey::ScmCommitButton
    } else {
        L10nKey::ScmCommitAllButton
    };
    let has_message = !message.trim().is_empty();
    // Mid-merge, an empty message is still committable: `ops` sends
    // `--allow-empty-message` for exactly this, because refusing would strand
    // the merge behind a message the user deliberately cleared.
    let merging = matches!(status.operation, Some(RepoOperation::Merge));
    let something = staged || tracked_edits || amend;
    CommitPlan {
        label,
        enabled: something && (has_message || amend || merging),
        reason: if something {
            L10nKey::ScmCommitNeedsMessage
        } else {
            L10nKey::ScmNothingToCommit
        },
    }
}

/// Whether a commit has to stage everything tracked first (`-a`).
pub(crate) fn commit_stages_everything(status: &WorkingTreeStatus, amend: bool) -> bool {
    // Amending with nothing staged means "fix the message", not "sweep the
    // working tree into the commit I already made".
    !amend && status.staged().next().is_none()
}

/// What a row's buttons do. Named rather than inlined because the same verb
/// appears on the row, on its group header and in its context menu, and the
/// three must not drift into meaning different things.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RowVerb {
    Discard,
    Stage,
    Unstage,
    OpenConflict,
    MarkResolved,
}

/// Right to left, most-used last: the pointer travels to the right edge, so
/// the button under it should be the one nine hovers out of ten want.
pub(crate) fn row_verbs(group: ScmGroup) -> &'static [(RowVerb, IconName)] {
    match group {
        ScmGroup::Merge => &[
            (RowVerb::OpenConflict, IconName::Eye),
            (RowVerb::MarkResolved, IconName::Check),
        ],
        ScmGroup::Staged => &[(RowVerb::Unstage, IconName::Minus)],
        ScmGroup::Changes | ScmGroup::Untracked => &[
            (RowVerb::Discard, IconName::Undo2),
            (RowVerb::Stage, IconName::Plus),
        ],
    }
}

/// The header's buttons are the row's, minus the ones that only make sense
/// for one file: there is no group-wide "open the conflict".
pub(crate) fn group_verbs(group: ScmGroup) -> &'static [(RowVerb, IconName)] {
    match group {
        ScmGroup::Merge => &[(RowVerb::MarkResolved, IconName::Check)],
        _ => row_verbs(group),
    }
}

/// The operation a verb runs over one or many paths. `None` for the verbs
/// that change no state.
pub(crate) fn verb_op(verb: RowVerb, group: ScmGroup, paths: Vec<RepoPath>) -> Option<GitOp> {
    if paths.is_empty() {
        return None;
    }
    Some(match verb {
        // Resolving a conflict is `git add`, exactly as it is on the command
        // line — there is no separate "resolve" verb in git.
        RowVerb::Stage | RowVerb::MarkResolved => GitOp::Stage { paths },
        RowVerb::Unstage => GitOp::Unstage { paths },
        RowVerb::Discard if group == ScmGroup::Untracked => {
            let directories = paths.iter().any(|p| p.as_str().ends_with('/'));
            GitOp::DiscardUntracked { paths, directories }
        }
        RowVerb::Discard => GitOp::DiscardWorktree { paths },
        RowVerb::OpenConflict => return None,
    })
}

/// The paths in a group git can actually be told about.
///
/// A path that is not valid UTF-8 cannot be sent as a pathspec at all, and
/// `GitOp::validate` rejects the whole operation over one of them — so a group
/// action has to leave them out rather than fail for everyone. Their own rows
/// are greyed out and say why.
pub(crate) fn writable_paths(entries: &[&StatusEntry]) -> Vec<RepoPath> {
    entries
        .iter()
        .filter(|e| e.path.pathspec().is_some())
        .map(|e| e.path.clone())
        .collect()
}

fn verb_tooltip(verb: RowVerb) -> &'static str {
    t(match verb {
        RowVerb::Discard => L10nKey::ScmDiscard,
        RowVerb::Stage => L10nKey::ScmStage,
        RowVerb::Unstage => L10nKey::ScmUnstage,
        RowVerb::OpenConflict => L10nKey::ScmOpenConflict,
        RowVerb::MarkResolved => L10nKey::ScmMarkResolved,
    })
}

fn verb_all_tooltip(verb: RowVerb) -> &'static str {
    t(match verb {
        RowVerb::Discard => L10nKey::ScmDiscardAll,
        RowVerb::Stage => L10nKey::ScmStageAll,
        RowVerb::Unstage => L10nKey::ScmUnstageAll,
        RowVerb::OpenConflict => L10nKey::ScmOpenConflict,
        RowVerb::MarkResolved => L10nKey::ScmMarkResolved,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::actions::{ScmCommit, ToggleFullscreen};
    use crate::core::config::{CoreConfig, DiffViewMode, RightPanelTab};
    use crate::ui::app::test_window::harness;
    use gpui::TestAppContext;
    use tty7_core::core::git::status::{ConflictKind, EntryKind, RepoPath};

    fn entry(path: &str, index: ChangeCode, worktree: ChangeCode, kind: EntryKind) -> StatusEntry {
        StatusEntry {
            path: RepoPath::from_bytes(path.as_bytes()),
            orig_path: None,
            index,
            worktree,
            kind,
            submodule: None,
            rename_score: None,
            conflict: (kind == EntryKind::Unmerged).then_some(ConflictKind::BothModified),
        }
    }

    fn groups_of(entry: &StatusEntry) -> Vec<ScmGroup> {
        ScmGroup::ORDER
            .into_iter()
            .filter(|g| in_group(entry, *g))
            .collect()
    }

    #[test]
    fn a_file_staged_and_edited_again_lands_in_both_groups() {
        let e = entry(
            "a.rs",
            ChangeCode::Modified,
            ChangeCode::Modified,
            EntryKind::Tracked,
        );
        assert_eq!(groups_of(&e), vec![ScmGroup::Staged, ScmGroup::Changes]);
    }

    #[test]
    fn each_other_kind_of_entry_lands_in_exactly_one_group() {
        let staged = entry(
            "a.rs",
            ChangeCode::Added,
            ChangeCode::None,
            EntryKind::Tracked,
        );
        assert_eq!(groups_of(&staged), vec![ScmGroup::Staged]);

        let unstaged = entry(
            "b.rs",
            ChangeCode::None,
            ChangeCode::Modified,
            EntryKind::Tracked,
        );
        assert_eq!(groups_of(&unstaged), vec![ScmGroup::Changes]);

        let untracked = entry(
            "c.rs",
            ChangeCode::None,
            ChangeCode::None,
            EntryKind::Untracked,
        );
        assert_eq!(groups_of(&untracked), vec![ScmGroup::Untracked]);

        // A conflict is only ever a conflict: it must not also show up under
        // Changes, or resolving it would look like two separate jobs.
        let conflict = entry(
            "d.rs",
            ChangeCode::Unmerged,
            ChangeCode::Unmerged,
            EntryKind::Unmerged,
        );
        assert_eq!(groups_of(&conflict), vec![ScmGroup::Merge]);
    }

    #[test]
    fn a_row_wears_the_letter_of_the_half_its_group_is_about() {
        // Added to the index, then edited again in the working tree.
        let e = entry(
            "a.rs",
            ChangeCode::Added,
            ChangeCode::Modified,
            EntryKind::Tracked,
        );
        assert_eq!(row_status(&e, ScmGroup::Staged), ("A", DecoStatus::Added));
        assert_eq!(
            row_status(&e, ScmGroup::Changes),
            ("M", DecoStatus::Modified)
        );

        let untracked = entry(
            "c.rs",
            ChangeCode::None,
            ChangeCode::None,
            EntryKind::Untracked,
        );
        assert_eq!(
            row_status(&untracked, ScmGroup::Untracked),
            ("?", DecoStatus::Untracked)
        );
        let conflict = entry(
            "d.rs",
            ChangeCode::Unmerged,
            ChangeCode::Unmerged,
            EntryKind::Unmerged,
        );
        assert_eq!(
            row_status(&conflict, ScmGroup::Merge),
            ("U", DecoStatus::Conflict)
        );
    }

    #[test]
    fn staged_rows_open_the_cached_diff_and_the_rest_the_working_tree() {
        assert_eq!(group_diff_source(ScmGroup::Staged), DiffSource::Staged);
        for group in [ScmGroup::Merge, ScmGroup::Changes, ScmGroup::Untracked] {
            assert_eq!(group_diff_source(group), DiffSource::Worktree);
        }
    }

    #[test]
    fn only_a_long_untracked_list_starts_folded() {
        assert!(!starts_collapsed(
            ScmGroup::Untracked,
            UNTRACKED_AUTO_COLLAPSE
        ));
        assert!(starts_collapsed(
            ScmGroup::Untracked,
            UNTRACKED_AUTO_COLLAPSE + 1
        ));
        for group in [ScmGroup::Merge, ScmGroup::Staged, ScmGroup::Changes] {
            assert!(!starts_collapsed(group, 1_000));
        }
    }

    #[test]
    fn every_button_a_row_offers_maps_to_the_verb_it_is_named_for() {
        let file = vec![RepoPath::from_bytes(b"a.rs")];
        assert!(matches!(
            verb_op(RowVerb::Stage, ScmGroup::Changes, file.clone()),
            Some(GitOp::Stage { .. })
        ));
        assert!(matches!(
            verb_op(RowVerb::Unstage, ScmGroup::Staged, file.clone()),
            Some(GitOp::Unstage { .. })
        ));
        // Resolving a conflict is `git add`; git has no other verb for it.
        assert!(matches!(
            verb_op(RowVerb::MarkResolved, ScmGroup::Merge, file.clone()),
            Some(GitOp::Stage { .. })
        ));
        assert!(matches!(
            verb_op(RowVerb::Discard, ScmGroup::Changes, file.clone()),
            Some(GitOp::DiscardWorktree { .. })
        ));
        // `checkout --` cannot restore a file git has never heard of.
        assert!(matches!(
            verb_op(RowVerb::Discard, ScmGroup::Untracked, file.clone()),
            Some(GitOp::DiscardUntracked {
                directories: false,
                ..
            })
        ));
        assert!(matches!(
            verb_op(
                RowVerb::Discard,
                ScmGroup::Untracked,
                vec![RepoPath::from_bytes(b"vendor/")]
            ),
            Some(GitOp::DiscardUntracked {
                directories: true,
                ..
            })
        ));
        assert!(verb_op(RowVerb::OpenConflict, ScmGroup::Merge, file).is_none());
        assert!(verb_op(RowVerb::Stage, ScmGroup::Changes, Vec::new()).is_none());
    }

    #[test]
    fn everything_that_can_lose_work_says_so_before_it_runs() {
        // The gate in `scm_op` keys off `destructive()`. If one of these ever
        // stopped reporting, the panel would throw the work away in silence.
        let paths = vec![RepoPath::from_bytes(b"a.rs")];
        for group in [ScmGroup::Changes, ScmGroup::Untracked] {
            let op = verb_op(RowVerb::Discard, group, paths.clone()).expect("discard has an op");
            assert!(op.destructive().is_some(), "{group:?} discard");
        }
        // Staging and unstaging are reversible, so they must not stop to ask.
        for verb in [RowVerb::Stage, RowVerb::Unstage, RowVerb::MarkResolved] {
            let op = verb_op(verb, ScmGroup::Changes, paths.clone()).expect("verb has an op");
            assert!(op.destructive().is_none(), "{verb:?}");
        }
    }

    #[test]
    fn a_group_action_leaves_out_the_paths_git_cannot_be_told_about() {
        let good = entry(
            "a.rs",
            ChangeCode::None,
            ChangeCode::Modified,
            EntryKind::Tracked,
        );
        let mut bad = good.clone();
        bad.path = RepoPath::from_bytes(&[0xff, 0xfe, b'.', b'r', b's']);
        assert!(bad.path.pathspec().is_none(), "the fixture must be lossy");

        let paths = writable_paths(&[&good, &bad]);
        assert_eq!(paths.len(), 1, "the lossy path is dropped, not carried");
        // One unrepresentable path would otherwise fail the operation for the
        // whole group.
        let op = verb_op(RowVerb::Stage, ScmGroup::Changes, paths).expect("still has work to do");
        assert!(op.validate().is_ok());

        let all_bad = verb_op(RowVerb::Stage, ScmGroup::Changes, writable_paths(&[&bad]));
        assert!(all_bad.is_none(), "nothing to do means no operation at all");
    }

    #[test]
    fn a_group_header_offers_no_button_that_only_makes_sense_for_one_file() {
        let verbs: Vec<RowVerb> = group_verbs(ScmGroup::Merge)
            .iter()
            .map(|(v, _)| *v)
            .collect();
        assert_eq!(verbs, vec![RowVerb::MarkResolved]);
        for group in [ScmGroup::Staged, ScmGroup::Changes, ScmGroup::Untracked] {
            for (verb, _) in group_verbs(group) {
                assert_ne!(*verb, RowVerb::OpenConflict);
            }
        }
    }

    /// The group chevron's box and the status letter's cell are one column.
    ///
    /// This is the detail that makes the list look drawn rather than
    /// assembled: every group arrow sits directly above the `M`s and `A`s of
    /// the rows it heads. The two widths live in two files — `git_badge` owns
    /// the letter's cell, this module owns the chevron's box — so nothing but
    /// this assertion stops one of them from moving on its own. They have to
    /// keep moving together.
    #[test]
    fn the_group_chevron_stands_in_the_status_letters_column() {
        assert_eq!(BADGE_W, crate::ui::right_panel::BADGE_W);
    }

    fn repo(root: &str) -> RepoKey {
        RepoKey {
            host: HostId::LOCAL,
            root: PathBuf::from(root),
        }
    }

    fn status_of_repo(root: &str, entries: Vec<StatusEntry>) -> WorkingTreeStatus {
        WorkingTreeStatus {
            root: PathBuf::from(root),
            home: PathBuf::from(root),
            head: HeadState::Branch {
                name: "main".into(),
                oid: "1111111".into(),
            },
            upstream: None,
            ahead_behind: None,
            total_entries: entries.len(),
            entries,
            truncated: false,
            stash_count: 0,
            operation: None,
            prefilled_message: None,
        }
    }

    #[test]
    fn the_commit_button_says_what_pressing_it_would_actually_do() {
        let staged = status_of_repo(
            "/a",
            vec![entry(
                "a.rs",
                ChangeCode::Modified,
                ChangeCode::None,
                EntryKind::Tracked,
            )],
        );
        let unstaged = status_of_repo(
            "/a",
            vec![entry(
                "a.rs",
                ChangeCode::None,
                ChangeCode::Modified,
                EntryKind::Tracked,
            )],
        );
        let clean = status_of_repo("/a", Vec::new());

        assert_eq!(
            commit_plan(&staged, false, "msg").label,
            L10nKey::ScmCommitButton
        );
        // Nothing staged: the button says so rather than quietly running -a.
        assert_eq!(
            commit_plan(&unstaged, false, "msg").label,
            L10nKey::ScmCommitAllButton
        );
        assert_eq!(
            commit_plan(&staged, true, "").label,
            L10nKey::ScmCommitAmendButton
        );

        assert!(commit_plan(&staged, false, "msg").enabled);
        assert!(
            !commit_plan(&staged, false, "   ").enabled,
            "an all-whitespace message is no message"
        );
        assert!(
            commit_plan(&clean, true, "").enabled,
            "amending with no message keeps HEAD's own with --no-edit"
        );
        assert!(!commit_plan(&clean, false, "msg").enabled);
    }

    #[test]
    fn only_a_commit_with_an_empty_index_sweeps_the_working_tree() {
        let staged = status_of_repo(
            "/a",
            vec![entry(
                "a.rs",
                ChangeCode::Modified,
                ChangeCode::None,
                EntryKind::Tracked,
            )],
        );
        let unstaged = status_of_repo(
            "/a",
            vec![entry(
                "a.rs",
                ChangeCode::None,
                ChangeCode::Modified,
                EntryKind::Tracked,
            )],
        );
        assert!(commit_stages_everything(&unstaged, false));
        assert!(!commit_stages_everything(&staged, false));
        // Amending is "fix the last commit", not "add everything to it".
        assert!(!commit_stages_everything(&unstaged, true));
    }

    #[test]
    fn a_branch_level_with_its_upstream_says_nothing() {
        assert_eq!(tracking_chip(Some("origin/main"), Some((0, 0))), None);
        // Both halves once there is anything to say, so the reading keeps its
        // shape as the numbers move.
        assert_eq!(
            tracking_chip(Some("origin/main"), Some((2, 0))).as_deref(),
            Some("↑2 ↓0")
        );
        assert_eq!(
            tracking_chip(Some("origin/main"), Some((0, 1))).as_deref(),
            Some("↑0 ↓1")
        );
        assert_eq!(
            tracking_chip(Some("origin/main"), Some((2, 1))).as_deref(),
            Some("↑2 ↓1")
        );
        // Nothing to compare against is not the same as being level: the chip
        // becomes the offer to publish.
        assert_eq!(
            tracking_chip(None, None).as_deref(),
            Some(t(L10nKey::ScmPublishBranch))
        );
        // An upstream git could not count against says nothing rather than
        // claiming zero.
        assert_eq!(tracking_chip(Some("origin/main"), None), None);
    }

    #[test]
    fn a_detached_head_shows_the_sha_git_would_print() {
        assert_eq!(
            head_label(&HeadState::Detached {
                oid: "0123456789abcdef".into()
            }),
            "0123456"
        );
        assert_eq!(
            head_label(&HeadState::Branch {
                name: "main".into(),
                oid: "0123456".into()
            }),
            "main"
        );
    }

    #[test]
    fn every_parked_operation_has_something_to_say() {
        // Interactive and plain rebase read the same on purpose: git writes
        // `rebase-merge/interactive` for both, so the distinction is not one
        // the repository on disk can make.
        assert_eq!(
            operation_label(RepoOperation::RebaseInteractive),
            operation_label(RepoOperation::Rebase)
        );
        for op in [
            RepoOperation::Merge,
            RepoOperation::Rebase,
            RepoOperation::CherryPick,
            RepoOperation::Revert,
            RepoOperation::Bisect,
            RepoOperation::Am,
        ] {
            assert!(!t(operation_label(op)).is_empty(), "{op:?}");
        }
    }

    #[gpui::test]
    fn commit_action_is_scoped_to_the_message_box(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let (_app, mut vcx) = harness(cx);

        let (in_box, outside, window_wide) = vcx.update(|window, _cx| {
            let scoped = gpui::KeyContext::parse(COMMIT_KEY_CONTEXT)
                .expect("the context the panel installs parses");
            (
                window.bindings_for_action_in_context(&ScmCommit, scoped),
                window.bindings_for_action_in_context(
                    &ScmCommit,
                    gpui::KeyContext::new_with_defaults(),
                ),
                window.bindings_for_action_in_context(
                    &ToggleFullscreen,
                    gpui::KeyContext::new_with_defaults(),
                ),
            )
        });

        assert!(
            !in_box.is_empty(),
            "the message box installs {COMMIT_KEY_CONTEXT}, and the binding has to live in it"
        );
        assert!(
            outside.is_empty(),
            "outside the box the chord must not commit anything"
        );
        assert!(
            !window_wide.is_empty(),
            "the window keeps its own binding for the same chord"
        );
        if cfg!(target_os = "macos") {
            // The whole point: one chord, two meanings, separated only by the
            // context the box installs. If they ever stopped colliding this
            // test would still pass for the wrong reason, so assert they do.
            assert_eq!(
                in_box[0].keystrokes(),
                window_wide[0].keystrokes(),
                "secondary-enter is shared, and scope is what tells them apart"
            );
        }
    }

    #[gpui::test]
    fn a_commit_draft_follows_its_repository(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let (app, mut vcx) = harness(cx);
        let (a, b) = (repo("/a"), repo("/b"));
        let status_a = status_of_repo("/a", Vec::new());
        let status_b = status_of_repo("/b", Vec::new());

        app.update_in(&mut vcx, |app, window, cx| {
            let input = app.scm_commit_input(&a, &status_a, window, cx);
            input.update(cx, |state, cx| state.set_value("wip: a", window, cx));

            let input = app.scm_commit_input(&b, &status_b, window, cx);
            assert_eq!(
                input.read(cx).value(),
                "",
                "another working tree is another message"
            );
            input.update(cx, |state, cx| state.set_value("wip: b", window, cx));

            let input = app.scm_commit_input(&a, &status_a, window, cx);
            assert_eq!(
                input.read(cx).value(),
                "wip: a",
                "coming back has to bring the draft with it"
            );
            assert_eq!(app.scm.draft(&b), "wip: b");
        });
    }

    #[gpui::test]
    fn a_merge_prefills_the_box_with_gits_own_message(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let (app, mut vcx) = harness(cx);
        let key = repo("/a");
        let mut status = status_of_repo("/a", Vec::new());
        status.prefilled_message = Some("Merge branch 'topic'".into());

        app.update_in(&mut vcx, |app, window, cx| {
            let input = app.scm_commit_input(&key, &status, window, cx);
            assert_eq!(input.read(cx).value(), "Merge branch 'topic'");
        });
    }

    #[gpui::test]
    fn a_rejected_commit_keeps_the_message_it_was_given(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let (app, mut vcx) = harness(cx);
        let key = repo("/a");
        let before = status_of_repo("/a", Vec::new());

        app.update(&mut vcx, |app, _cx| {
            app.scm.committing = Some((key.clone(), before.head.clone(), "wip".into()));

            // A pre-commit hook said no: HEAD has not moved, so the message
            // the user wrote is still the only copy of it there is.
            assert!(!app.scm_commit_landed(&key, &before, "wip"));

            let mut after = before.clone();
            after.head = HeadState::Branch {
                name: "main".into(),
                oid: "2222222".into(),
            };
            assert!(app.scm_commit_landed(&key, &after, "wip"));
            assert_eq!(app.scm.draft(&key), "");
        });
    }

    fn tab_from(json: &str) -> RightPanelTab {
        serde_json::from_str::<CoreConfig>(json)
            .expect("config deserializes")
            .right_panel_tab
    }

    #[gpui::test]
    fn scm_tab_opens_and_config_round_trips(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let (app, mut vcx) = harness(cx);

        app.update(&mut vcx, |app, cx| {
            app.set_right_panel_tab(RightPanelTab::Scm, cx)
        });
        let (visible, tab) = app.read_with(&vcx, |app, _| {
            (app.right_panel_visible, app.right_panel_tab)
        });
        assert!(visible);
        assert_eq!(tab, RightPanelTab::Scm);

        // The whole reason the variant was renamed in place: what lands on
        // disk is still `"changes"`, so a build from before this change reads
        // its own config back and stays on the panel the user left open.
        let cfg = CoreConfig {
            right_panel_tab: RightPanelTab::Scm,
            ..Default::default()
        };
        let json = serde_json::to_value(&cfg).expect("config serializes");
        assert_eq!(json["right_panel_tab"], serde_json::json!("changes"));

        assert_eq!(
            tab_from(r#"{"right_panel_tab":"changes"}"#),
            RightPanelTab::Scm
        );
        assert_eq!(tab_from(r#"{"right_panel_tab":"scm"}"#), RightPanelTab::Scm);
        assert_eq!(tab_from(r#"{"right_panel_tab":"git"}"#), RightPanelTab::Scm);
        // Anything unrecognised falls back through `de_lenient` rather than
        // failing the whole file.
        assert_eq!(
            tab_from(r#"{"right_panel_tab":"nonsense"}"#),
            RightPanelTab::Info
        );
    }

    #[gpui::test]
    fn the_new_config_fields_default_to_todays_behaviour(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let (app, mut vcx) = harness(cx);

        let cfg = CoreConfig::default();
        assert_eq!(cfg.diff_view, DiffViewMode::Split);
        assert!(!cfg.scm_graph_expanded);

        // Toggling has to survive the round trip through the config, since the
        // panel reads it back on the next launch.
        app.update(&mut vcx, |app, cx| app.scm_toggle_graph(cx));
        assert!(app.read_with(&vcx, |app, _| app.scm.graph.expanded));
        assert!(vcx.update(|_, cx| {
            cx.global::<crate::core::config::Config>()
                .scm_graph_expanded
        }));

        app.update(&mut vcx, |app, cx| app.toggle_diff_view_mode(cx));
        assert_eq!(
            vcx.update(|_, cx| cx.global::<crate::core::config::Config>().diff_view),
            DiffViewMode::Unified
        );
    }

    #[gpui::test]
    fn a_folded_group_stays_folded_across_rerenders(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let (app, mut vcx) = harness(cx);

        app.update(&mut vcx, |app, cx| {
            app.set_right_panel_tab(RightPanelTab::Scm, cx);
            app.scm_toggle_group(ScmGroup::Staged, 3, cx);
        });
        vcx.background_executor.run_until_parked();
        vcx.run_until_parked();
        assert!(app.read_with(&vcx, |app, _| app.scm.group_collapsed(ScmGroup::Staged, 3)));

        // And a long untracked list that the user opened by hand stays open,
        // rather than snapping shut again on the count.
        app.update(&mut vcx, |app, cx| {
            app.scm_toggle_group(ScmGroup::Untracked, 500, cx)
        });
        vcx.run_until_parked();
        assert!(!app.read_with(&vcx, |app, _| {
            app.scm.group_collapsed(ScmGroup::Untracked, 500)
        }));
    }
}

/// The panel asks git for a lot, from inside `render`. These hold it to
/// asking once and then going quiet.
///
/// The hazard is specific: `scm_refresh` reaches for its cache through
/// `default_global`, which fires the global observers whether or not anything
/// changed, and it is called every frame. A watcher that notified on every one
/// of those would request a frame from inside a frame and never stop.
#[cfg(all(test, unix))]
mod render_idle_gpui_tests {
    use super::*;
    use crate::daemon::protocol::DaemonMsg;
    use crate::ui::app::{render_probe, test_window};
    use crate::ui::host_ops::HostId;
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use tty7_core::core::config::RightPanelTab;

    const BUDGET: u64 = 200;

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tty7-scm-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(&dir).unwrap()
    }

    fn git(root: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?} failed");
    }

    fn scm_panel_on(
        cx: &mut TestAppContext,
        root: &Path,
        until: impl Fn(&Tty7App, &gpui::App) -> bool,
    ) -> (
        Entity<Tty7App>,
        VisualTestContext,
        std::os::unix::net::UnixStream,
    ) {
        let (app, mut vcx, mut pane) = test_window::harness_with_pane(cx);
        DaemonMsg::Cwd(root.to_path_buf())
            .encode(&mut pane)
            .expect("the pane's socket takes the cwd");
        app.update_in(&mut vcx, |app, _, cx| {
            app.right_panel_visible = true;
            app.right_panel_tab = RightPanelTab::Scm;
            cx.notify();
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            app.update_in(&mut vcx, |_, _, cx| cx.notify());
            vcx.background_executor.run_until_parked();
            if app.update_in(&mut vcx, |app, _, cx| until(app, cx)) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the panel never settled on the directory"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        test_window::quiesce(&mut vcx, Some(root));
        (app, vcx, pane)
    }

    fn draws_while_idle(vcx: &mut VisualTestContext) -> u64 {
        test_window::quiesce(vcx, None);
        render_probe::arm(BUDGET);
        vcx.background_executor.run_until_parked();
        vcx.executor()
            .advance_clock(std::time::Duration::from_secs(3));
        vcx.background_executor.run_until_parked();
        render_probe::arm(BUDGET);
        // No real-time exposure in the counted window, deliberately. The file
        // tree holds a real filesystem watch, so real time is a channel input
        // arrives on — and a test that spends it here is asking to be handed
        // some. What has to be waited out is waited out in `quiesce` above,
        // where a frame costs nothing.
        vcx.executor()
            .advance_clock(std::time::Duration::from_secs(9));
        vcx.background_executor.run_until_parked();
        render_probe::draws()
    }

    #[gpui::test]
    fn a_settled_source_control_panel_reaches_render_idle(cx: &mut TestAppContext) {
        let _serial = serial();
        let root = scratch("settled");
        git(&root, &["init", "--quiet"]);
        std::fs::write(root.join("a.rs"), "fn main() {}\n").unwrap();
        git(&root, &["add", "a.rs"]);
        std::fs::write(root.join("b.rs"), "// untracked\n").unwrap();

        let want = root.clone();
        let (app, mut vcx, _pane) = scm_panel_on(cx, &root, move |app, cx| {
            app.scm.repo.as_ref().is_some_and(|r| r.root == want)
                && crate::terminal::git_data::status_of(cx, HostId::LOCAL, &want).is_some()
        });

        let status = app.update_in(&mut vcx, |app, _, cx| {
            crate::terminal::git_data::status_of(
                cx,
                HostId::LOCAL,
                &app.scm.repo.clone().unwrap().root,
            )
        });
        let status = status.expect("the panel read a status");
        assert_eq!(status.staged().count(), 1, "a.rs is in the index");
        assert_eq!(status.untracked().count(), 1, "b.rs is not");

        assert_eq!(draws_while_idle(&mut vcx), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[gpui::test]
    fn a_directory_with_no_repository_reaches_render_idle(cx: &mut TestAppContext) {
        let _serial = serial();
        let root = scratch("bare");
        std::fs::write(root.join("notes.txt"), "").unwrap();

        // Nothing here is a repository, so `git status` must never be reached
        // — and the panel must not spin looking for one that is not coming.
        let want = root.clone();
        let (app, mut vcx, _pane) = scm_panel_on(cx, &root, move |app, _cx| {
            app.scm.roots.contains_key(&(HostId::LOCAL, want.clone()))
        });
        assert!(
            app.update_in(&mut vcx, |app, _, _| app
                .scm
                .roots
                .values()
                .all(|(_, r)| r.is_none())),
            "no root was resolved for a directory that is not a repository"
        );
        assert!(
            app.update_in(&mut vcx, |app, _, _| app.scm.repo.is_none()),
            "and no status was ever asked for"
        );
        assert_eq!(draws_while_idle(&mut vcx), 0);
        let _ = std::fs::remove_dir_all(&root);
    }
}

/// The action strip is the one place in the panel where a hover reveal sits
/// under the pointer it is reacting to, so it is the one place where "is the
/// row hovered" and "can the row's hitbox be seen from here" can disagree.
#[cfg(test)]
mod action_strip_gpui_tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use super::*;
    use gpui::{
        Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels,
        PlatformInput, Point, Render, TestAppContext, VisualTestContext, point,
    };

    /// A file row in miniature: a group, something to hover on the left, the
    /// real strip on the right with one button-sized child in it, and the
    /// row's own click handler — the one that opens the diff overlay, and the
    /// one a click on the strip must never reach.
    #[derive(Default)]
    struct Row {
        row_clicks: Rc<Cell<usize>>,
    }

    impl Render for Row {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let id = SharedString::from("row");
            let clicks = self.row_clicks.clone();
            // Off the origin, because a fresh test window's pointer sits at
            // (0, 0) and a row under it would start out hovered.
            div().pt(px(50.)).pl(px(50.)).child(
                div()
                    .id(id.clone())
                    .group(id.clone())
                    .relative()
                    .w(px(300.))
                    .h(px(ROW_H))
                    .on_click(move |_, _, _| clicks.set(clicks.get() + 1))
                    .child(
                        action_strip(&id, 0x00EEEEEE).child(
                            div()
                                .w(px(TILE_SIZE_XS))
                                .h(px(TILE_SIZE_XS))
                                .debug_selector(|| "scm-action".into()),
                        ),
                    ),
            )
        }
    }

    fn hover(vcx: &mut VisualTestContext, at: Point<Pixels>) {
        vcx.update(|window, cx| {
            window.dispatch_event(
                PlatformInput::MouseMove(MouseMoveEvent {
                    position: at,
                    pressed_button: None,
                    modifiers: Modifiers::none(),
                }),
                cx,
            );
            window.refresh();
        });
        vcx.run_until_parked();
    }

    fn click(vcx: &mut VisualTestContext, at: Point<Pixels>) {
        hover(vcx, at);
        vcx.update(|window, cx| {
            window.dispatch_event(
                PlatformInput::MouseDown(MouseDownEvent {
                    button: MouseButton::Left,
                    position: at,
                    modifiers: Modifiers::none(),
                    click_count: 1,
                    first_mouse: false,
                }),
                cx,
            );
            window.dispatch_event(
                PlatformInput::MouseUp(MouseUpEvent {
                    button: MouseButton::Left,
                    position: at,
                    modifiers: Modifiers::none(),
                    click_count: 1,
                }),
                cx,
            );
            window.refresh();
        });
        vcx.run_until_parked();
    }

    #[gpui::test]
    fn the_strip_survives_the_pointer_reaching_it(cx: &mut TestAppContext) {
        let window = cx.add_window(|_, _| Row::default());
        let mut vcx = VisualTestContext::from_window(window.into(), cx);
        vcx.run_until_parked();
        assert!(
            vcx.debug_bounds("scm-action").is_none(),
            "an unhovered row draws no buttons"
        );

        hover(&mut vcx, point(px(70.), px(62.)));
        let strip = vcx
            .debug_bounds("scm-action")
            .expect("hovering the row reveals the buttons");

        // The whole point of the strip: the pointer travels right, onto it.
        hover(&mut vcx, strip.center());
        assert!(
            vcx.debug_bounds("scm-action").is_some(),
            "the buttons are still drawn with the pointer on top of them"
        );
    }

    /// What `occlude()` used to buy, and what replaced it.
    #[gpui::test]
    fn a_click_on_the_strip_never_reaches_the_row(cx: &mut TestAppContext) {
        let row = Row::default();
        let clicks = row.row_clicks.clone();
        let window = cx.add_window(|_, _| row);
        let mut vcx = VisualTestContext::from_window(window.into(), cx);
        vcx.run_until_parked();

        click(&mut vcx, point(px(70.), px(62.)));
        assert_eq!(clicks.get(), 1, "clicking the row opens the diff");

        let strip = vcx
            .debug_bounds("scm-action")
            .expect("the row is hovered, so the strip is up");
        click(&mut vcx, strip.center());
        assert_eq!(
            clicks.get(),
            1,
            "but a click on the strip belongs to the strip"
        );
    }
}

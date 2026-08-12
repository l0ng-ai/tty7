//! One commit, in the panel's own body.
//!
//! Replacing the body rather than opening a third kind of container: the
//! panel already knows how to draw a list of changed files, and the only
//! things a commit adds above it are its message and who wrote it. The file
//! rows are the same rows, minus the buttons — nothing here should be able to
//! drift from what the working tree shows.
//!
//! The patch itself still goes to the full-screen overlay. 260px is not a
//! place to read a diff.
//!
//! This is also where the panel pays back what the graph gave up. A history
//! row has about 26 characters beside its lanes and this repository's subjects
//! run to a median of 64, so the graph shows shape and this shows text: the
//! whole subject, the body, every ref, the parents, how many lines moved, and
//! the files.
//!
//! What it is *not* is a container of its own. It is drawn flush with the
//! panel, on the panel's fill, in the panel's type scale — the same dense,
//! low-chrome language as every other body the right panel shows. A
//! second-level view announces itself by the way back at the top of it and by
//! being the only thing on screen; it does not need a raised card, a larger
//! ramp or filled tokens to say so, and an earlier round that gave it all
//! three read as a foreign design pasted into the app. A later round tried the
//! ramp on its own — 14/12/11.5 through the whole panel — and it read the same
//! way: too big for a 260px column, and too loud beside the graph. The sizes
//! below are the panel's own, and this view has no business being a step above
//! them.

use std::sync::Arc;

use gpui::{AnyElement, Context, SharedString, Window, div, prelude::*, px};
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};

use tty7_core::core::git::diff::CommitLabel;
use tty7_core::core::git::log::{Commit, CommitFile, RefKind};
use tty7_core::core::git::status::DecoStatus;

use crate::terminal::git_diff::DiffSource;
use crate::ui::app::{CONTENT_INSET, Tty7App};
use crate::ui::i18n::{L10nKey, t, t_plural};
use crate::ui::right_panel::{ROW_INSET, git_badge, info_chip};
use crate::ui::scm::path::{relative_time, split_display_path};
use crate::ui::scm::state::{CommitDetailView, RepoKey};
use crate::ui::scm::status::{status_color, status_glyph};

/// A file row, the same height as the working tree's, and inset the same way.
/// The two lists sit in one column and have to read as one grid.
///
/// Both numbers are a 12px row's. gpui leads a plain `div` at phi, so the row's
/// mono name occupies `round(12 × 1.618) = 19px`, and 24 gives that line 2.5px
/// of air on each side — dense, which is what a 260px column of paths wants.
/// The text lands on `CONTENT_INSET` whatever [`ROW_INSET`] is, since the list
/// subtracts it outside the row and the row adds it back inside — which is why
/// the inset itself is the panel-wide constant rather than a third copy here: a
/// reader who opens a commit must not feel the pitch change under them.
const ROW_H: f32 = 24.;

/// How much of the body is shown before it folds. Four lines is a paragraph;
/// past that it is a changelog, and the file list is what the reader came for.
const BODY_LINES: usize = 4;

/// And how much of the subject, which wraps rather than folding. Three lines
/// of 12px in 260px is around 90 characters — longer than every subject in
/// this repository but a handful, and a cap for the ones that are a paragraph.
const SUBJECT_LINES: usize = 3;

/// The panel's type ramp, named rather than spelled out at each of its dozen
/// uses. These are not this view's sizes to choose: they are the steps the
/// right panel runs on, and the whole point of naming them here is that a
/// future edit changes a constant instead of drifting one line of the body off
/// the ramp.
///
/// 12px is body text and the loudest thing on screen — the subject, and the
/// way back, which is the same size worn quietly. 11px is everything that
/// qualifies it: the byline, the message body, the parents' label, the file
/// count, the waiting notes. One point of difference is all a qualifier needs
/// in a column this narrow; the separation is carried by weight and colour,
/// not by the gap. 10.5px mono is the token size — sha-like strings, ref
/// chips, the diff counts, and `git_badge`'s status letter. That one is barely
/// a choice made here: `git_badge` and `info_chip` set their own mono at 10.5,
/// and an object id has to read as the same kind of thing in this view as it
/// does in the graph and on a file row.
///
/// Emphasis in this panel is weight and colour rather than size, and a fill
/// only where it carries a meaning of its own — HEAD, and a tag. The subject is
/// a step up in weight against the full foreground while everything under it is
/// muted, and that separation is all it needs; it is what a card was briefly
/// and wrongly asked to do, and what a larger ramp was later asked to do after
/// that. Both were turned down. Nothing in this view is bigger than the
/// working tree's rows are.
///
/// `right_panel.rs` is where the panel's own steps live, and these are that
/// ramp under local names that say what each step does *here* — subject,
/// qualifier, token. If the two ever disagree, that file is the one that is
/// right.
///
/// The changed-file rows deliberately do not read these. They spell 12 and 11
/// out because they have to stay pixel-identical to `scm_file_row` in
/// `panel.rs`, and a constant shared with the prose above would let a change
/// here silently break that.
const SUBJECT_SIZE: f32 = 12.;
const SECONDARY_SIZE: f32 = 11.;
const TOKEN_SIZE: f32 = 10.5;

impl Tty7App {
    /// Show one commit, replacing the working tree in the panel body.
    ///
    /// `seed` is the commit the caller already has. The graph's page carries
    /// every field this view renders, so a click on a row hands its own
    /// [`Commit`] over and no `git show` is run at all; a parent link, or
    /// anything else reaching a commit outside that window, passes `None` and
    /// pays for the read.
    pub(crate) fn open_commit_detail(
        &mut self,
        repo: RepoKey,
        oid: String,
        seed: Option<Commit>,
        cx: &mut Context<Self>,
    ) {
        self.scm.detail = Some(CommitDetailView::new(repo, oid, seed));
        cx.notify();
    }

    pub(crate) fn close_commit_detail(&mut self, cx: &mut Context<Self>) {
        if self.scm.detail.take().is_some() {
            cx.notify();
        }
    }

    /// The commit detail body, shown in place of the file groups.
    pub(crate) fn render_commit_detail(
        &mut self,
        detail: &CommitDetailView,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        // The detail names its own repository, and the panel may since have
        // followed the active pane somewhere else. A commit from a repository
        // nobody is looking at any more is not a second-level view of
        // anything, so it goes rather than sitting on top of the wrong tree.
        if self.scm.active_repo() != Some(&detail.repo) {
            self.scm.detail = None;
            return None;
        }
        self.load_commit_detail(detail, cx);

        let mono = cx.theme().mono_font_family.clone();
        let muted = cx.theme().muted_foreground;
        // No surface, no margin: this is the panel's body while a commit is
        // open, and it starts where every other panel body starts.
        //
        // Each section insets itself by `CONTENT_INSET` rather than sharing one
        // on the column, because the rows that want a hover fill lay themselves
        // out a `ROW_INSET` short of it so the fill is wider than the text, and
        // an outer inset would have to be undone by every one of them.
        let mut body = v_flex()
            .py(px(2.))
            .child(self.detail_header_row(detail, &mono, cx));

        match detail.commit.as_deref() {
            Some(commit) => {
                body = body
                    .child(self.detail_message(detail, commit, cx))
                    .children(self.detail_refs(commit, &mono, cx))
                    .children(self.detail_parents(detail, commit, &mono, cx))
                    .child(self.detail_files(detail, commit, &mono, cx));
            }
            // Nothing came back. `loaded` is what tells "still reading" apart
            // from "git has no such commit here" — without it a bad oid would
            // read as a spinner that never stops.
            None => {
                body = body.child(
                    div()
                        .px(px(CONTENT_INSET))
                        .py(px(4.))
                        .text_size(px(SECONDARY_SIZE))
                        .text_color(muted)
                        .child(if detail.loaded {
                            t(L10nKey::ScmCommitNotFound)
                        } else {
                            t(L10nKey::PanelLoading)
                        }),
                );
            }
        }
        Some(body.into_any_element())
    }

    /// Read the commit and its file list, once.
    ///
    /// Runs from `render`, so it has to be idempotent in the strongest sense:
    /// the panel is redrawn on every status change and a second dispatch would
    /// mean a `git show` per frame. `loading` covers the window while a read
    /// is out and `loaded` covers every frame after it lands, including the
    /// ones where it landed with nothing.
    fn load_commit_detail(&mut self, detail: &CommitDetailView, cx: &mut Context<Self>) {
        if detail.loading || detail.loaded {
            return;
        }
        let Some(host) = crate::ui::host_registry::HostRegistry::get(cx, detail.repo.host) else {
            return;
        };
        if let Some(open) = self.scm.detail.as_mut() {
            open.loading = true;
        }
        let root = detail.repo.root.clone();
        let oid = detail.oid.clone();
        // A seeded view already has its metadata and only wants the files, so
        // the `show` is skipped rather than run for an answer we hold.
        let seeded = detail.commit.is_some();
        let key = (detail.repo.clone(), detail.oid.clone());
        crate::ui::host_ops::HostOps::run(
            host,
            cx,
            move |h| {
                use tty7_core::core::git::log;
                let commit = (!seeded)
                    .then(|| log::load_commit(h, &root, &oid))
                    .flatten();
                (commit, log::commit_files(h, &root, &oid))
            },
            move |this, (commit, files), cx| {
                // The user may have gone back, or moved on to another commit,
                // while the read was out. Landing it anywhere but on the view
                // that asked would show one commit's files under another's
                // message.
                let Some(open) = this
                    .scm
                    .detail
                    .as_mut()
                    .filter(|d| (d.repo.clone(), d.oid.clone()) == key)
                else {
                    return;
                };
                open.loading = false;
                open.loaded = true;
                if let Some(commit) = commit {
                    open.commit = Some(Arc::new(commit));
                }
                match files {
                    Some(files) => open.files = Some(Arc::new(files)),
                    // A failed read is not an empty commit — see
                    // `CommitDetailView::files_failed`.
                    None => open.files_failed = true,
                }
                cx.notify();
            },
        );
    }

    /// The way back, and the object id.
    ///
    /// The back affordance belongs in `panel_title`'s trailing slot, where the
    /// diff overlay puts its own. It is here instead because the title is
    /// rendered by the panel and this function only produces the body — see
    /// the note in `render_panel_scm`. Being the first row of the body it
    /// scrolls with the content, which is the one thing lost by the move.
    ///
    /// Both halves of the row are chrome and are drawn as chrome: muted, no
    /// resting fill, the hover doing all the work of saying they are hit
    /// targets. The loudest text in this view has to be the subject — the row
    /// above it is a way out and a string to copy, and neither is what the
    /// reader came to read. The oid is set in the same mono at the same token
    /// size as the parent links below, so the two read as the same kind of
    /// thing.
    fn detail_header_row(
        &self,
        detail: &CommitDetailView,
        mono: &SharedString,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let oid = detail.oid.clone();
        let hover_bg = gpui::rgb(panel_surface(cx).hover);
        let muted = cx.theme().muted_foreground;
        h_flex()
            .items_center()
            .gap(px(4.))
            .h(px(ROW_H))
            .px(px(CONTENT_INSET - ROW_INSET))
            .child(
                h_flex()
                    .id("scm-detail-back")
                    .items_center()
                    .gap(px(2.))
                    .px(px(4.))
                    .py(px(1.))
                    .rounded(px(4.))
                    .cursor_pointer()
                    .hover(|s| s.bg(hover_bg))
                    .on_click(cx.listener(|this, _, _window, cx| this.close_commit_detail(cx)))
                    // `small()`, which is 14px of glyph beside a 12px label —
                    // an icon needs a little more box than the text it labels
                    // to read as the same size, and it is the width of
                    // `git_badge`'s cell as well.
                    .child(Icon::new(IconName::ChevronLeft).small().text_color(muted))
                    .child(
                        div()
                            .text_size(px(SUBJECT_SIZE))
                            .text_color(muted)
                            .child(t(L10nKey::ScmBackToChanges)),
                    ),
            )
            .child(div().flex_1().min_w_0())
            .child(
                div()
                    .id("scm-detail-sha")
                    .flex_none()
                    .px(px(4.))
                    .py(px(1.))
                    .rounded(px(4.))
                    .cursor_pointer()
                    .hover(|s| s.bg(hover_bg))
                    .text_size(px(TOKEN_SIZE))
                    .text_color(muted)
                    .font_family(mono.clone())
                    .tooltip(|window, cx| {
                        gpui_component::tooltip::Tooltip::new(t(L10nKey::ScmCopyCommitSha))
                            .build(window, cx)
                    })
                    .on_click(move |_, _window, cx| {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(oid.clone()));
                    })
                    .child(short_oid(&detail.oid).to_string()),
            )
            .into_any_element()
    }

    /// Subject, byline, body.
    fn detail_message(
        &self,
        detail: &CommitDetailView,
        commit: &Commit,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let body = commit.body.trim();
        let lines = body.lines().count();
        let folded = !detail.body_expanded && lines > BODY_LINES;
        v_flex()
            .px(px(CONTENT_INSET))
            .pb(px(4.))
            .gap(px(3.))
            .child(
                // Wrapping, not truncating: this view exists because the graph
                // row could only show the first 26 characters. It carries the
                // weight and the full foreground while everything under it is
                // muted, and that is the whole of its emphasis — it sits on
                // the same 12px step as the file rows below it.
                div()
                    .text_size(px(SUBJECT_SIZE))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(cx.theme().foreground)
                    .line_clamp(SUBJECT_LINES)
                    .child(SharedString::from(commit.summary.clone())),
            )
            .child(
                div()
                    .text_size(px(SECONDARY_SIZE))
                    .text_color(cx.theme().muted_foreground)
                    .child(byline(commit, now_unix())),
            )
            .when(!body.is_empty(), |this| {
                this.child(
                    // The secondary size — the same one the byline above it
                    // and the parents below it are set in. A size of its own
                    // bought nothing and cost the reader a fourth step in a
                    // view eight lines tall.
                    div()
                        .pt(px(2.))
                        .text_size(px(SECONDARY_SIZE))
                        .text_color(cx.theme().muted_foreground)
                        .when(folded, |d| d.line_clamp(BODY_LINES))
                        .child(SharedString::from(body.to_string())),
                )
                .when(lines > BODY_LINES, |this| {
                    this.child(
                        div()
                            .id("scm-detail-body-fold")
                            .w_full()
                            .py(px(1.))
                            .cursor_pointer()
                            .text_size(px(SECONDARY_SIZE))
                            .text_color(cx.theme().info)
                            .on_click(cx.listener(|this, _, _window, cx| {
                                if let Some(open) = this.scm.detail.as_mut() {
                                    open.body_expanded = !open.body_expanded;
                                    cx.notify();
                                }
                            }))
                            .child(t(if folded {
                                L10nKey::ScmShowMore
                            } else {
                                L10nKey::ScmShowLess
                            })),
                    )
                })
            })
            .into_any_element()
    }

    /// Every ref pointing here, wrapped over as many lines as it takes.
    ///
    /// The graph row shows one chip and a `+N`; there is no reason to hide any
    /// of them once there is a whole column to put them in.
    ///
    /// Exactly two of them get a fill, and they are the two that mean
    /// something. HEAD is where you are, washed in `accent` under the full
    /// foreground — one emphasised token on the row. A tag is yellow because a
    /// tag is yellow everywhere in git. Everything else — the other local
    /// branches, every remote-tracking ref — is a bare muted span: no fill, and
    /// therefore no padding either, because padding exists to hold text off a
    /// background and there is no background to hold it off. A ref name is
    /// already a word with spaces around it; drawing a box around every one of
    /// them turns a list of names into a wall of blocks, which is what the
    /// panel's language is trying not to be.
    ///
    /// `theme.accent` is a neutral surface tint in tty7 rather than the brand
    /// colour, which is exactly why 0.28 of it works: it is a raised patch, not
    /// a wash of hue, and the foreground stays legible on it. Do not substitute
    /// `theme.ring` here and then have to drop the opacity to compensate.
    ///
    /// What this must never go back to is the bug that predated all of it: the
    /// fallback arm painted `theme.accent` at *full* opacity under muted text,
    /// which made `origin/main` louder than the branch you were actually on and
    /// left HEAD looking like the footnote.
    fn detail_refs(
        &self,
        commit: &Commit,
        mono: &SharedString,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if commit.refs.is_empty() {
            return None;
        }
        let theme = cx.theme();
        let (accent, warning, fg, muted) = (
            theme.accent,
            theme.warning,
            theme.foreground,
            theme.muted_foreground,
        );
        let mut row = h_flex()
            .flex_wrap()
            .items_center()
            // Six, not four: most of these are bare words now, and words need a
            // little more air between them than chips whose fills already say
            // where one ends and the next begins.
            .gap(px(6.))
            .px(px(CONTENT_INSET))
            .pb(px(6.));
        for deco in &commit.refs {
            row = row.child(match deco.kind {
                RefKind::Tag => info_chip(&deco.short, warning.opacity(0.16), warning, mono),
                _ if deco.is_head => info_chip(&deco.short, accent.opacity(0.28), fg, mono),
                _ => ref_span(&deco.short, muted, mono),
            });
        }
        Some(row.into_any_element())
    }

    /// The parents, as links. Following one is the only way to walk history
    /// backwards from a commit the graph's window does not reach.
    ///
    /// `theme.info` and nothing else at rest — the panel's link ink, the same
    /// one the "show more" fold uses a few lines above. A filled pill would
    /// make the parents a second block competing with the ref chips, and this
    /// is a link, not a state. The hover fill is what says the oid is a target,
    /// and it is the same fill, radius and inset the header's two affordances
    /// use. The oids themselves are token-sized mono, matching the sha in the
    /// header so that every object id in this view is one recognisable shape.
    fn detail_parents(
        &self,
        detail: &CommitDetailView,
        commit: &Commit,
        mono: &SharedString,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if commit.parents.is_empty() {
            return None;
        }
        let hover_bg = gpui::rgb(panel_surface(cx).hover);
        let (muted, link) = {
            let theme = cx.theme();
            (theme.muted_foreground, theme.info)
        };
        let mut row = h_flex()
            .flex_wrap()
            .items_center()
            .gap(px(6.))
            .px(px(CONTENT_INSET))
            .pb(px(4.))
            .child(
                div()
                    .text_size(px(SECONDARY_SIZE))
                    .text_color(muted)
                    .child(t(L10nKey::ScmCommitParents)),
            );
        for parent in &commit.parents {
            let repo = detail.repo.clone();
            let oid = parent.clone();
            row = row.child(
                div()
                    .id(SharedString::from(format!("scm-detail-parent-{parent}")))
                    .px(px(4.))
                    .py(px(1.))
                    .rounded(px(4.))
                    .cursor_pointer()
                    .hover(|s| s.bg(hover_bg))
                    .text_size(px(TOKEN_SIZE))
                    .font_family(mono.clone())
                    .text_color(link)
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        // No seed: a parent is by definition one step past
                        // whatever the caller had in hand.
                        this.open_commit_detail(repo.clone(), oid.clone(), None, cx);
                    }))
                    .child(short_oid(parent).to_string()),
            );
        }
        Some(row.into_any_element())
    }

    /// The summary line and the rows under it.
    ///
    /// While the read is out there is no summary line at all. The count used to
    /// come from `unwrap_or_default()` and so said "no files changed" for as
    /// long as it took git to answer, which is a claim rather than a wait.
    fn detail_files(
        &self,
        detail: &CommitDetailView,
        commit: &Commit,
        mono: &SharedString,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(files) = detail.files.clone() else {
            let note = if detail.files_failed {
                L10nKey::ScmDetailFilesFailed
            } else {
                L10nKey::PanelLoading
            };
            return self.detail_note(t(note).to_string(), cx);
        };
        let list = v_flex().child(self.detail_summary(&files, mono, cx));
        // The label rides along on the source so the overlay's header can say
        // which commit it is showing, and it is deliberately not part of that
        // source's identity — the same commit opened from here and from
        // anywhere else has to stay one overlay.
        let source = DiffSource::Commit {
            rev: detail.oid.clone(),
            label: Some(CommitLabel {
                subject: commit.summary.clone(),
                author: commit.author.name.clone(),
                at: commit.author.at.unix,
            }),
        };
        // The rows sit in the working tree's own column: laid out one
        // `ROW_INSET` short of `CONTENT_INSET` and padding themselves back out,
        // so a hovered row's background is wider than its text.
        let mut rows = v_flex().px(px(CONTENT_INSET - ROW_INSET));
        for file in files.iter() {
            rows = rows.child(self.detail_file_row(detail, &source, file, mono, cx));
        }
        list.child(rows).into_any_element()
    }

    /// `12 files changed  +340 −118`.
    ///
    /// How much the commit is, in one line: the count in words and the size in
    /// numbers. The counts are the only place in this view where green and red
    /// appear, which is what lets them be read without a legend — and they are
    /// mono so the two columns of digits line up against the counts the diff
    /// overlay shows for the same commit, where the reader is going next.
    ///
    /// `−` is U+2212, not a hyphen, matching every other count and gutter mark
    /// in the diff views: the ASCII one sits too high and too short beside a
    /// `+` of the same size.
    ///
    /// Still not `panel_subtitle`: that helper uppercases its label and puts
    /// anything in its trailing slot hard against the right edge, because the
    /// slot was built for a button. Both are wrong here. "3 FILES CHANGED" is
    /// a heading's voice and this is a sentence about the commit, and the
    /// counts are not a control off in the corner — they qualify the words and
    /// have to sit next to them, which is the one thing the layout round got
    /// right and the user asked to keep.
    ///
    /// What the helper *is* copied on is its frame: the hairline and the six
    /// above it, so the file list starts on exactly the line the working tree's
    /// does. A rule is how this panel divides sections; the round that replaced
    /// it with a raised card is the round being undone.
    ///
    /// The two paddings are that frame re-derived rather than copied, because
    /// the tallest line in each block is a different size. gpui leads a plain
    /// `div` at phi: the helper's 10.5px uppercase label measures
    /// `round(10.5 × 1.618) = 17px`, and the tallest thing in this row is the
    /// 11px file count at `round(11 × 1.618) = 18`. The helper's block is
    /// `6 + 1 + 12 + 17 + 4 = 40px` tall, so this one has 15px of padding to
    /// spend instead of 16 — half a pixel off each side, which keeps the total
    /// at 40 *and* puts both lines' optical centre 27.5px below the top of the
    /// margin, so nothing shifts when the reader opens a commit. Change either
    /// side's type and this has to be worked out again on both, or one list
    /// quietly starts a pixel or two below the other and nobody can see why.
    fn detail_summary(
        &self,
        files: &[CommitFile],
        mono: &SharedString,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (muted, border) = (theme.muted_foreground, theme.border);
        let (added_ink, removed_ink) = (theme.success, theme.danger);
        let counts = diff_totals(files);
        h_flex()
            .items_center()
            .gap(px(6.))
            .mt(px(6.))
            .border_t_1()
            .border_color(border)
            .px(px(CONTENT_INSET))
            .pt(px(11.5))
            .pb(px(3.5))
            .child(
                div()
                    .text_size(px(SECONDARY_SIZE))
                    .text_color(muted)
                    .child(t_plural(L10nKey::ScmFilesChanged, files.len(), &[])),
            )
            .when_some(counts, |this, (added, removed)| {
                this.child(
                    h_flex()
                        .items_center()
                        .gap(px(5.))
                        .text_size(px(TOKEN_SIZE))
                        .font_family(mono.clone())
                        .child(div().text_color(added_ink).child(format!("+{added}")))
                        .child(div().text_color(removed_ink).child(format!("−{removed}"))),
                )
            })
            .into_any_element()
    }

    /// The working tree's file row, minus the hover buttons.
    ///
    /// A copy of `scm_file_row`, which is the wrong way round and known to be:
    /// the two have to stay pixel-identical and nothing here enforces that.
    /// They differ only in what they are built from — a `StatusEntry` against
    /// a [`CommitFile`] — and in the buttons, so the shared version is a
    /// function over `(letter, deco, path)` plus an optional trailing element.
    fn detail_file_row(
        &self,
        detail: &CommitDetailView,
        source: &DiffSource,
        file: &CommitFile,
        mono: &SharedString,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let sf = panel_surface(cx);
        let deco = crate::ui::diff_overlay::deco_status(file.status);
        let (name, dir) = split_display_path(&file.path);
        let selected = self.diff_overlay_focus(detail.repo.host, &detail.repo.root, source)
            == Some(file.path.as_str());

        h_flex()
            .id(SharedString::from(format!("scm-detail-file-{}", file.path)))
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
                let repo = detail.repo.clone();
                let source = source.clone();
                let path = file.path.clone();
                cx.listener(move |this, _, window, cx| {
                    // 260px cannot render a patch, so the file level is the
                    // full-screen overlay's job — the same one the working
                    // tree's rows open, pointed at a commit instead.
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
            .child(git_badge(status_glyph(deco), status_color(deco, cx), mono))
            .child(
                div()
                    .flex_none()
                    // 12 and, below, 11: written out rather than taken from
                    // `SUBJECT_SIZE` and `SECONDARY_SIZE`, which happen to
                    // hold the same two numbers. The prose above this list is
                    // free to move off the ramp one day; a row is not, because
                    // it has to stay pixel-identical to `scm_file_row`, and
                    // sharing a constant with the prose is exactly how that
                    // would break without anybody touching this function.
                    //
                    // Its counterpart spells the same two out for the same
                    // reason, and says so in a comment pointing back here.
                    // They are the SCM panel's own 12/11px steps (the panel
                    // has not moved onto the interface font scale yet); a move
                    // of that ramp has to be carried into both rows by hand,
                    // and nothing but these two comments says so.
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
            .into_any_element()
    }

    fn detail_note(&self, text: String, cx: &mut Context<Self>) -> AnyElement {
        div()
            .px(px(CONTENT_INSET))
            .py(px(3.))
            .text_size(px(SECONDARY_SIZE))
            .text_color(cx.theme().muted_foreground.opacity(0.75))
            .child(text)
            .into_any_element()
    }
}

/// The surface every hover and selection in this view is computed against.
///
/// The sidebar's, because the right panel is the sidebar and this view paints
/// nothing under itself. A fill derived from `window` would be a step off a
/// base that is not there, and `list_hover` is the popover's.
fn panel_surface(cx: &gpui::App) -> crate::ui::presets::Surface {
    cx.global::<crate::ui::presets::Surfaces>().sidebar
}

/// One unemphasised ref, as bare text.
///
/// The counterpart to `info_chip` for the arm that has no fill: same mono, same
/// size, no padding and no radius, so a row of ordinary refs reads as a row of
/// words rather than a row of empty boxes. `flex_none` because the row wraps
/// and a ref name must break between names, never inside one.
fn ref_span(text: &str, ink: gpui::Hsla, mono: &SharedString) -> AnyElement {
    div()
        .flex_none()
        .text_size(px(TOKEN_SIZE))
        .font_family(mono.clone())
        .text_color(ink)
        .child(text.to_string())
        .into_any_element()
}

/// The commit's line delta, summed over the files git was able to count.
///
/// The numbers are already in hand: `commit_files` joins `--numstat` against
/// `--name-status`, so every [`CommitFile`] arrives carrying its own `added`
/// and `removed`. Nothing extra is read to draw this line.
///
/// `None` means there is nothing worth printing, which is two cases wearing
/// one answer. A binary file has no counts at all — `--numstat` prints `-` for
/// both columns and the fields come through as [`None`] — so a commit that
/// only touched binaries sums to zero out of zero. A pure rename does have
/// counts, and they are `0` and `0`. Either way `+0 −0` is a measurement of
/// nothing, and a line of type that answers a question nobody asked; the file
/// rows already say what happened.
///
/// Summed with `saturating_add` rather than `sum()`, which panics on overflow
/// in a debug build. This runs in `render`, and a repository that manages four
/// billion added lines in one commit should get a wrong number, not a crash.
pub(crate) fn diff_totals(files: &[CommitFile]) -> Option<(u32, u32)> {
    let fold = |pick: fn(&CommitFile) -> Option<u32>| {
        files
            .iter()
            .filter_map(pick)
            .fold(0u32, |acc, n| acc.saturating_add(n))
    };
    let (added, removed) = (fold(|f| f.added), fold(|f| f.removed));
    (added > 0 || removed > 0).then_some((added, removed))
}

/// `Ada Lovelace · 2h`. Author, not committer: a rebase rewrites the second
/// one, and "who wrote this" is the question a reader is asking.
pub(crate) fn byline(commit: &Commit, now: i64) -> String {
    let when = (commit.author.at.unix > 0).then(|| relative_time(now, commit.author.at.unix));
    match (commit.author.name.trim(), when) {
        ("", Some(when)) => when,
        (name, Some(when)) => format!("{name} · {when}"),
        (name, None) => name.to_string(),
    }
}

/// Seven, which is what git itself prints and what the graph's rows use.
pub(crate) fn short_oid(oid: &str) -> &str {
    &oid[..oid.len().min(7)]
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tty7_core::core::git::diff::FileStatus;
    use tty7_core::core::git::log::{OffsetTs, Signature};

    fn commit(name: &str, at: i64) -> Commit {
        Commit {
            oid: "3f2a1b9c8d7e6f5a4b3c2d1e0f9a8b7c6d5e4f3a".into(),
            parents: Default::default(),
            author: Signature {
                name: name.into(),
                email: "ada@example.com".into(),
                at: OffsetTs {
                    unix: at,
                    offset_minutes: 0,
                },
            },
            committer: Signature {
                name: "Grace".into(),
                email: "grace@example.com".into(),
                at: OffsetTs {
                    unix: at,
                    offset_minutes: 0,
                },
            },
            summary: "s".into(),
            body: String::new(),
            refs: Vec::new(),
        }
    }

    #[test]
    fn the_byline_drops_the_separator_along_with_the_half_it_joined() {
        let now = 1_786_255_391 + 7200;
        assert_eq!(byline(&commit("Ada", 1_786_255_391), now), "Ada · 2h");
        assert_eq!(
            byline(&commit("", 1_786_255_391), now),
            "2h",
            "an unattributed commit is not `· 2h`"
        );
        assert_eq!(
            byline(&commit("Ada", 0), now),
            "Ada",
            "and a date that would not parse is not `Ada · 56y`"
        );
    }

    #[test]
    fn a_short_oid_is_the_seven_characters_git_itself_prints() {
        assert_eq!(
            short_oid("3f2a1b9c8d7e6f5a4b3c2d1e0f9a8b7c6d5e4f3a"),
            "3f2a1b9"
        );
        assert_eq!(short_oid("abc"), "abc", "a truncated oid is not padded");
        assert_eq!(short_oid(""), "");
    }

    fn file(path: &str, status: FileStatus, counts: Option<(u32, u32)>) -> CommitFile {
        CommitFile {
            path: path.into(),
            orig_path: None,
            status,
            added: counts.map(|c| c.0),
            removed: counts.map(|c| c.1),
            binary: counts.is_none(),
        }
    }

    #[test]
    fn the_summary_adds_up_every_file_git_could_count() {
        let files = [
            file("a.rs", FileStatus::Modified, Some((10, 4))),
            file("b.rs", FileStatus::Added, Some((2, 0))),
            file("c.rs", FileStatus::Deleted, Some((0, 8))),
        ];
        assert_eq!(diff_totals(&files), Some((12, 12)));
    }

    /// The two shapes of "there is nothing to print", which have to come back
    /// as the same answer even though git spells them differently: `-` for a
    /// binary, and a real pair of zeroes for a rename.
    #[test]
    fn a_commit_with_nothing_countable_gets_no_counts_rather_than_zeroes() {
        let binary = [file("logo.png", FileStatus::Modified, None)];
        assert_eq!(
            diff_totals(&binary),
            None,
            "`--numstat` printed `-`, so there is no number to show"
        );

        let renamed = [file("new.rs", FileStatus::Renamed, Some((0, 0)))];
        assert_eq!(
            diff_totals(&renamed),
            None,
            "a pure rename is counted, and what it counts to is nothing"
        );

        assert_eq!(diff_totals(&[]), None, "and neither is an empty list");
    }

    /// A binary alongside real edits must not swallow them, and must not be
    /// counted as a zero that drags the total down either — it simply is not
    /// part of the sum.
    #[test]
    fn an_uncountable_file_drops_out_of_a_sum_that_still_has_something_in_it() {
        let mixed = [
            file("logo.png", FileStatus::Added, None),
            file("main.rs", FileStatus::Modified, Some((3, 1))),
        ];
        assert_eq!(diff_totals(&mixed), Some((3, 1)));
    }

    /// `render` calls this, so an absurd repository has to give a wrong number
    /// rather than take the frame down with it.
    #[test]
    fn a_total_past_what_a_u32_holds_saturates_instead_of_panicking() {
        let files = [
            file("a.rs", FileStatus::Modified, Some((u32::MAX, 1))),
            file("b.rs", FileStatus::Modified, Some((7, u32::MAX))),
        ];
        assert_eq!(diff_totals(&files), Some((u32::MAX, u32::MAX)));
    }
}

/// The detail view against a real repository, drawn in a real window.
///
/// Construction alone would prove very little: everything that can go wrong
/// here — a missing global, a theme token, a slice through the middle of a
/// character — goes wrong during layout and paint, so these arm the render
/// probe and insist something was actually drawn.
#[cfg(all(test, unix))]
mod detail_gpui_tests {
    use super::*;
    use crate::daemon::protocol::DaemonMsg;
    use crate::ui::app::{render_probe, test_window};
    use crate::ui::host_ops::HostId;
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use std::path::{Path, PathBuf};
    use tty7_core::core::config::RightPanelTab;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tty7-detail-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(&dir).unwrap()
    }

    fn git(root: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?} failed");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Two commits: a root, then one that renames a file, adds a path with a
    /// space in it and writes a body long enough to fold.
    ///
    /// HEAD also carries a tag and a second branch, so that a frame drawn over
    /// it exercises all three arms of `detail_refs` — the filled HEAD chip, the
    /// filled tag chip and the bare `ref_span` — rather than only the one the
    /// current branch happens to take. The fallback arm is where the ordinary
    /// refs used to be painted louder than HEAD, so it is the arm most worth
    /// putting through layout and paint.
    fn two_commit_repo(name: &str) -> PathBuf {
        let root = scratch(name);
        git(&root, &["init", "--quiet"]);
        git(&root, &["config", "user.email", "ada@example.com"]);
        git(&root, &["config", "user.name", "Ada"]);
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        git(&root, &["add", "a.txt"]);
        git(&root, &["commit", "-qm", "root commit"]);
        std::fs::rename(root.join("a.txt"), root.join("renamed.txt")).unwrap();
        std::fs::write(root.join("with space.txt"), "two\n").unwrap();
        std::fs::write(root.join("中文名.txt"), "three\n").unwrap();
        git(&root, &["add", "-A"]);
        git(
            &root,
            &[
                "commit",
                "-qm",
                "feat(detail): a subject long enough that the graph row could never have shown it",
                "-m",
                "one\ntwo\nthree\nfour\nfive\nsix",
            ],
        );
        git(&root, &["branch", "sidequest"]);
        git(&root, &["tag", "v1.0.0"]);
        root
    }

    fn panel_on(
        cx: &mut TestAppContext,
        root: &Path,
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
        let want = root.to_path_buf();
        settle(&app, &mut vcx, move |app, _| {
            app.scm.repo.as_ref().is_some_and(|r| r.root == want)
        });
        (app, vcx, pane)
    }

    /// Pump frames until the panel has done what it was asked. The panel only
    /// starts a read from `render`, so nothing here can be awaited directly.
    fn settle(
        app: &Entity<Tty7App>,
        vcx: &mut VisualTestContext,
        done: impl Fn(&Tty7App, &gpui::App) -> bool,
    ) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            app.update_in(vcx, |_, _, cx| cx.notify());
            vcx.background_executor.run_until_parked();
            if app.update_in(vcx, |app, _, cx| done(app, cx)) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the panel never settled"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    fn paths(app: &Entity<Tty7App>, vcx: &mut VisualTestContext) -> Vec<String> {
        app.update_in(vcx, |app, _, _| {
            app.scm
                .detail
                .as_ref()
                .and_then(|d| d.files.clone())
                .map(|files| files.iter().map(|f| f.path.clone()).collect())
                .unwrap_or_default()
        })
    }

    #[gpui::test]
    fn a_commit_detail_reads_its_own_files_and_draws_them(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let root = two_commit_repo("draws");
        let head = git(&root, &["rev-parse", "HEAD"]);
        let (app, mut vcx, _pane) = panel_on(cx, &root);

        let repo = app.update_in(&mut vcx, |app, _, _| app.scm.repo.clone().unwrap());
        app.update_in(&mut vcx, |app, _, cx| {
            app.open_commit_detail(repo.clone(), head.clone(), None, cx)
        });
        settle(&app, &mut vcx, |app, _| {
            app.scm.detail.as_ref().is_some_and(|d| d.loaded)
        });

        // Nothing was seeded, so the metadata came from `git show`.
        app.update_in(&mut vcx, |app, _, _| {
            let detail = app.scm.detail.as_ref().expect("the detail is open");
            let commit = detail.commit.as_ref().expect("git resolved the commit");
            assert_eq!(commit.oid, head);
            assert!(commit.summary.starts_with("feat(detail):"));
            assert_eq!(commit.parents.len(), 1);
            assert_eq!(commit.body.lines().count(), 6, "long enough to fold");

            // The three arms of `detail_refs`, so the frame drawn below is
            // known to have gone through all of them and not just the first.
            let refs = &commit.refs;
            assert!(refs.iter().any(|r| r.is_head), "the checked-out branch");
            assert!(
                refs.iter().any(|r| r.kind == RefKind::Tag),
                "the tag: {refs:?}"
            );
            assert!(
                refs.iter().any(|r| !r.is_head && r.kind != RefKind::Tag),
                "and a plain ref, which is the arm drawn without a fill: {refs:?}"
            );
        });
        let mut listed = paths(&app, &mut vcx);
        listed.sort();
        assert_eq!(
            listed,
            ["renamed.txt", "with space.txt", "中文名.txt"],
            "the two -z streams joined into one list"
        );

        // The summary's counts come off the same list, with nothing else read
        // for them. Rename detection is a git config away from changing what
        // the individual rows say, so this asserts the shape rather than the
        // arithmetic: two files of one line each were added, so there is a
        // number to print and it is not zero.
        app.update_in(&mut vcx, |app, _, _| {
            let files = app.scm.detail.as_ref().unwrap().files.clone().unwrap();
            let (added, removed) = diff_totals(&files).expect("a text commit has counts");
            assert!(
                added >= 2,
                "the added lines were summed: +{added} −{removed}"
            );
        });

        // A real frame, so layout and paint run over every row above.
        render_probe::arm(10_000);
        app.update_in(&mut vcx, |_, _, cx| cx.notify());
        vcx.background_executor.run_until_parked();
        assert!(
            render_probe::draws() > 0,
            "nothing was drawn, so nothing was proved"
        );

        // Expanding the body is another branch of the same element.
        app.update_in(&mut vcx, |app, _, cx| {
            app.scm.detail.as_mut().unwrap().body_expanded = true;
            cx.notify();
        });
        render_probe::arm(10_000);
        app.update_in(&mut vcx, |_, _, cx| cx.notify());
        vcx.background_executor.run_until_parked();
        assert!(render_probe::draws() > 0);

        // The read runs from `render`, which is the shape that has spun this
        // panel before: a dispatch that did not record itself would ask git
        // for the same commit again on the frame its own answer caused.
        assert_eq!(draws_while_idle(&mut vcx), 0);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Copied from `panel.rs`'s own idle tests: arm the probe, let every timer
    /// the panel owns fire, and count the frames nobody asked for.
    fn draws_while_idle(vcx: &mut VisualTestContext) -> u64 {
        render_probe::arm(200);
        vcx.background_executor.run_until_parked();
        vcx.executor()
            .advance_clock(std::time::Duration::from_secs(3));
        vcx.background_executor.run_until_parked();
        render_probe::arm(200);
        vcx.executor()
            .advance_clock(std::time::Duration::from_secs(9));
        vcx.background_executor.run_until_parked();
        render_probe::draws()
    }

    #[gpui::test]
    fn following_a_parent_swaps_the_commit_and_going_back_clears_it(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let root = two_commit_repo("parent");
        let head = git(&root, &["rev-parse", "HEAD"]);
        let parent = git(&root, &["rev-parse", "HEAD^"]);
        let (app, mut vcx, _pane) = panel_on(cx, &root);

        let repo = app.update_in(&mut vcx, |app, _, _| app.scm.repo.clone().unwrap());
        app.update_in(&mut vcx, |app, _, cx| {
            app.open_commit_detail(repo.clone(), head.clone(), None, cx)
        });
        settle(&app, &mut vcx, |app, _| {
            app.scm.detail.as_ref().is_some_and(|d| d.loaded)
        });

        // What the parent link does: the same call with the other oid, and
        // nothing carried over from the commit that was on screen.
        app.update_in(&mut vcx, |app, _, cx| {
            app.open_commit_detail(repo.clone(), parent.clone(), None, cx)
        });
        app.update_in(&mut vcx, |app, _, _| {
            let detail = app.scm.detail.as_ref().unwrap();
            assert_eq!(detail.oid, parent);
            assert!(detail.commit.is_none(), "the old commit did not linger");
            assert!(!detail.loaded);
        });
        settle(&app, &mut vcx, |app, _| {
            app.scm.detail.as_ref().is_some_and(|d| d.loaded)
        });
        assert_eq!(
            paths(&app, &mut vcx),
            ["a.txt"],
            "a root commit's files are what it added, with no --root needed"
        );

        app.update_in(&mut vcx, |app, _, cx| app.close_commit_detail(cx));
        assert!(app.update_in(&mut vcx, |app, _, _| app.scm.detail.is_none()));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[gpui::test]
    fn a_seeded_detail_only_asks_for_the_files(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let root = two_commit_repo("seeded");
        let head = git(&root, &["rev-parse", "HEAD"]);
        let (app, mut vcx, _pane) = panel_on(cx, &root);
        let repo = app.update_in(&mut vcx, |app, _, _| app.scm.repo.clone().unwrap());

        // What the graph hands over: a row it already holds. The subject is
        // deliberately not the real one, so a `git show` behind our back would
        // overwrite it and show up here.
        let mut seed = tty7_core::core::git::log::load_commit(
            &*tty7_core::host::local::LocalHost::new(),
            &root,
            &head,
        )
        .expect("the scratch repo answers");
        seed.summary = "what the graph already knew".into();
        app.update_in(&mut vcx, |app, _, cx| {
            app.open_commit_detail(repo.clone(), head.clone(), Some(seed), cx)
        });
        settle(&app, &mut vcx, |app, _| {
            app.scm.detail.as_ref().is_some_and(|d| d.loaded)
        });

        app.update_in(&mut vcx, |app, _, _| {
            let detail = app.scm.detail.as_ref().unwrap();
            assert_eq!(
                detail.commit.as_ref().unwrap().summary,
                "what the graph already knew",
                "the seed was kept, so no second read of the same commit happened"
            );
            assert_eq!(detail.files.as_ref().unwrap().len(), 3);
        });
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A commit from a repository the panel has since walked away from is not
    /// a second-level view of anything.
    #[gpui::test]
    fn a_detail_from_another_repository_gives_the_body_back(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let root = two_commit_repo("elsewhere");
        let head = git(&root, &["rev-parse", "HEAD"]);
        let (app, mut vcx, _pane) = panel_on(cx, &root);

        app.update_in(&mut vcx, |app, window, cx| {
            let stranger = RepoKey {
                host: HostId::LOCAL,
                root: PathBuf::from("/no/such/tty7/repo"),
            };
            app.open_commit_detail(stranger.clone(), head.clone(), None, cx);
            let detail = app.scm.detail.clone().unwrap();
            assert!(app.render_commit_detail(&detail, window, cx).is_none());
            assert!(app.scm.detail.is_none(), "and it does not come back");
        });
        let _ = std::fs::remove_dir_all(&root);
    }
}

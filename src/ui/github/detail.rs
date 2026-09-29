//! One issue or pull request, pushed into the GitHub tab over its list.
//!
//! Title, state, author, labels, the body and the conversation as Markdown —
//! through gpui-component's `TextView`, after `core::github::markdown` has
//! turned images into links and disarmed non-web link targets — and, for a
//! pull request, its branches, where it stands on merging, its checks, its
//! reviewers and its changed files. A file opens in the diff overlay, the
//! same surface a commit's files open in, fed the patch GitHub sent rather
//! than one git read.

use std::sync::Arc;

use gpui::{AnyElement, Context, SharedString, Window, div, prelude::*, px, rems};
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};

use tty7_core::core::git::diff::{CommitLabel, DiffBudget, DiffSnapshot, DiffSource};
use tty7_core::core::github::{
    Check, CheckState, Checks, Comment, Detail, PrFile, Readiness, RepoSlug, ReviewState, Reviewer,
    readiness,
};

use crate::ui::app::{CONTENT_INSET, Tty7App};
use crate::ui::github::{Fold, now_unix};
use crate::ui::i18n::{L10nKey, t, t_fmt, t_plural};
use crate::ui::panel_github::{
    check_glyph, check_label, describe_error, github_tile, label_chip, state_glyph, state_label,
};
use crate::ui::right_panel::{
    HEADING, META, META_MONO, ROW_FILL_RADIUS, ROW_INSET, TEXT, TEXT_INSET, git_badge,
};
use crate::ui::scm::path::{relative_time, split_display_path};
use crate::ui::scm::state::RepoKey;
use crate::ui::scm::status::{status_color, status_glyph};

const ROW_H: f32 = 26.;
const SECTION_GAP: f32 = 16.;
/// Past this many checks, the ones that passed or were skipped fold into one
/// row: a repository with a build matrix has dozens, and the one that failed
/// is what the section is read for.
const CHECKS_FOLD_AT: usize = 5;
/// Past this many changed files, only the first [`FILES_SHOWN_FOLDED`] show
/// until the list is unfolded — the conversation under it stays in reach.
const FILES_FOLD_AT: usize = 10;
const FILES_SHOWN_FOLDED: usize = 8;
/// Past this many reviewers, only the first few show.
const REVIEWERS_FOLD_AT: usize = 5;
/// Past this many comments, the middle folds away the way GitHub's own
/// timeline does: the opening one stays, and the latest few.
const COMMENTS_FOLD_AT: usize = 4;
const COMMENTS_KEPT_LATEST: usize = 2;
/// A description or comment longer than this is clamped to [`CLAMP_HEIGHT`]
/// until unfolded. Judged on the source, since the rendered height is only
/// known after layout; a pasted log or a long template trips it, a few
/// paragraphs do not.
const CLAMP_LINES: usize = 18;
const CLAMP_CHARS: usize = 1600;
const CLAMP_HEIGHT: f32 = 16.;

impl Tty7App {
    /// The rows pinned over the detail, and the detail itself.
    pub(crate) fn render_github_detail(
        &mut self,
        repo: &RepoKey,
        slug: &RepoSlug,
        number: u64,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (Vec<AnyElement>, AnyElement) {
        self.github_ensure_detail(slug, number, cx);
        let key = (slug.clone(), number);
        let (detail, error) = match self.github.details.get(&key) {
            Some(c) => (c.detail.clone(), c.error.clone()),
            None => (None, None),
        };
        let is_pr = detail.as_ref().map_or(
            self.github.kind == tty7_core::core::github::Kind::Pulls,
            |d| d.item.is_pr,
        );
        let url = detail.as_ref().map_or_else(
            || {
                format!(
                    "{}/{}/{number}",
                    slug.web_url(),
                    if is_pr { "pull" } else { "issues" }
                )
            },
            |d| d.item.html_url.clone(),
        );
        let pinned = vec![self.github_back_row(repo, number, is_pr, url, cx)];

        let Some(detail) = detail else {
            let body = match error {
                Some(err) => {
                    let (text, hint) = describe_error(&err, self.github_authenticated());
                    self.panel_empty(&text, hint.as_deref(), cx)
                }
                None => self.panel_empty(t(L10nKey::PanelLoading), None, cx),
            };
            return (pinned, body);
        };

        let mut body = v_flex()
            .pb(px(16.))
            .child(self.github_detail_head(&detail, cx));
        // What a pull request is usually opened to find out — can it go in,
        // and if not, what is it waiting for — before what it says.
        if let Some(checks) = detail.checks.as_ref().filter(|c| !c.items.is_empty()) {
            let unfolded = self.github_unfolded(slug, number, Fold::Checks);
            let toggle = self.github_fold_toggle(slug, number, Fold::Checks, cx);
            body = body.child(checks_section(
                checks,
                &detail.item.html_url,
                unfolded,
                toggle,
                cx,
            ));
        }
        if let Some(reviewers) = detail.reviewers.as_ref().filter(|r| !r.is_empty()) {
            let unfolded = self.github_unfolded(slug, number, Fold::Reviews);
            let toggle = self.github_fold_toggle(slug, number, Fold::Reviews, cx);
            body = body.child(reviews_section(reviewers, unfolded, toggle, cx));
        }
        body = body.child(self.github_long_markdown(
            format!("gh-body-{}-{number}", slug.full()),
            &detail.body,
            t(L10nKey::GitHubNoDescription),
            slug,
            number,
            Fold::Body,
            cx,
        ));
        if let Some(err) = error {
            // A refresh that failed over a detail already on screen: keep the
            // detail, and say why it may be out of date.
            let (text, hint) = describe_error(&err, self.github_authenticated());
            body = body.child(self.panel_empty(&text, hint.as_deref(), cx));
        }
        if let Some(files) = &detail.files {
            body = body.child(self.github_detail_files(repo, slug, &detail, files, cx));
        }
        body = body.child(self.github_detail_comments(slug, number, &detail, cx));
        (pinned, body.into_any_element())
    }

    fn github_unfolded(&self, slug: &RepoSlug, number: u64, fold: Fold) -> bool {
        self.github.unfolded.contains(&(slug.clone(), number, fold))
    }

    fn github_fold_toggle(
        &self,
        slug: &RepoSlug,
        number: u64,
        fold: Fold,
        cx: &mut Context<Self>,
    ) -> impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static {
        let key = (slug.clone(), number, fold);
        cx.listener(move |this, _, _window, cx| {
            if !this.github.unfolded.remove(&key) {
                this.github.unfolded.insert(key.clone());
            }
            cx.notify();
        })
    }

    fn github_back_row(
        &self,
        repo: &RepoKey,
        number: u64,
        is_pr: bool,
        url: String,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let sf = cx.global::<crate::ui::presets::Surfaces>().sidebar;
        let muted = cx.theme().muted_foreground;
        h_flex()
            .flex_none()
            .items_center()
            .gap(px(4.))
            .h(px(28.))
            .px(px(CONTENT_INSET))
            .child(
                h_flex()
                    .id("panel-github-back")
                    .items_center()
                    .gap(px(2.))
                    .h(px(ROW_H - 4.))
                    .pl(px(ROW_INSET - 2.))
                    .pr(px(ROW_INSET))
                    .rounded(px(6.))
                    .cursor_pointer()
                    .hover(|s| s.bg(gpui::rgb(sf.hover)))
                    .on_click(cx.listener(|this, _, _window, cx| this.github_close_detail(cx)))
                    .child(Icon::new(IconName::ChevronLeft).small().text_color(muted))
                    .child(
                        div()
                            .text_size(rems(TEXT))
                            .text_color(muted)
                            .child(t(if is_pr {
                                L10nKey::GitHubPulls
                            } else {
                                L10nKey::GitHubIssues
                            })),
                    ),
            )
            .child(div().flex_1().min_w_0())
            .child(
                div()
                    .text_size(rems(META_MONO))
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_color(muted)
                    .child(format!("#{number}")),
            )
            .child(self.github_refresh_tile(Some(repo.clone()), cx))
            .child(
                github_tile(
                    "panel-github-open-item",
                    Icon::new(IconName::ExternalLink),
                    t(L10nKey::GitHubOpenOnGitHub),
                    cx,
                )
                .on_click(move |_, _window, cx| cx.open_url(&url)),
            )
            .into_any_element()
    }

    /// Title, state, byline, branches and size, labels.
    fn github_detail_head(&self, detail: &Detail, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, fg) = (theme.muted_foreground, theme.foreground);
        let (added_ink, removed_ink) = (theme.success, theme.danger);
        let mono = theme.mono_font_family.clone();
        let item = &detail.item;
        let now = now_unix();
        // Both times, each named: the list row shows the last update, and a
        // bare "4d" here beside a "13h" there read as the two disagreeing.
        let when = |key, at| t_fmt(key, &[("when", &relative_time(now, at))]);
        let mut byline: Vec<String> = Vec::new();
        if !item.author.is_empty() {
            byline.push(item.author.clone());
        }
        if item.created_at > 0 {
            byline.push(when(L10nKey::GitHubOpenedAt, item.created_at));
        }
        // An update within a minute of opening is the opening itself.
        if item.updated_at > item.created_at + 60 {
            byline.push(when(L10nKey::GitHubUpdatedAt, item.updated_at));
        }
        let byline = byline.join(" · ");
        let mut head = v_flex()
            .px(px(TEXT_INSET))
            .pt(px(4.))
            .pb(px(10.))
            .gap(px(6.))
            .child(
                div()
                    .text_size(rems(TEXT + 1. / 16.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(fg)
                    .child(item.title.clone()),
            )
            .child(
                h_flex()
                    .items_center()
                    .gap(px(6.))
                    .flex_wrap()
                    .child(
                        h_flex()
                            .flex_none()
                            .items_center()
                            .gap(px(4.))
                            .px(px(6.))
                            .py(px(1.))
                            .rounded(px(10.))
                            .bg(theme.foreground.opacity(0.06))
                            .child(state_glyph(item.state, item.is_pr, cx))
                            .child(
                                div()
                                    .text_size(rems(META))
                                    .text_color(fg)
                                    .child(state_label(item.state)),
                            ),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .text_size(rems(META))
                            .text_color(muted)
                            .child(byline),
                    ),
            );
        if let Some(pull) = &detail.pull {
            head = head.child(
                h_flex()
                    .items_center()
                    .flex_wrap()
                    .gap(px(6.))
                    .text_size(rems(META_MONO))
                    .font_family(mono.clone())
                    .text_color(muted)
                    .child(format!("{} → {}", pull.head_ref, pull.base_ref))
                    .child(
                        div()
                            .text_color(added_ink)
                            .child(format!("+{}", pull.additions)),
                    )
                    .child(
                        div()
                            .text_color(removed_ink)
                            .child(format!("−{}", pull.deletions)),
                    )
                    .child(
                        div()
                            .font_family(theme.font_family.clone())
                            .text_size(rems(META))
                            .child(t_plural(L10nKey::GitHubCommits, pull.commits as usize, &[])),
                    ),
            );
            let ready = readiness(
                item.state,
                pull.merge_state,
                detail.checks.as_ref(),
                detail.reviewers.as_deref(),
            );
            if let Some(ready) = ready {
                head = head.child(readiness_line(ready, cx));
            }
        }
        if !item.labels.is_empty() {
            head = head.child(
                h_flex()
                    .flex_wrap()
                    .gap(px(4.))
                    .children(item.labels.iter().map(|l| label_chip(l, cx))),
            );
        }
        head.into_any_element()
    }

    fn github_detail_comments(
        &self,
        slug: &RepoSlug,
        number: u64,
        detail: &Detail,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let fg = cx.theme().foreground;
        let now = now_unix();
        let count = detail.comments.len().max(detail.item.comments as usize);
        let mut section = v_flex().mt(px(SECTION_GAP)).child(section_heading(
            t_plural(L10nKey::GitHubComments, count, &[]),
            cx,
        ));
        let total = detail.comments.len();
        let foldable = total > COMMENTS_FOLD_AT;
        let unfolded = self.github_unfolded(slug, number, Fold::Comments);
        let hidden = if foldable && !unfolded {
            1..total - COMMENTS_KEPT_LATEST
        } else {
            0..0
        };
        for (i, c) in detail.comments.iter().enumerate() {
            if hidden.contains(&i) {
                if i == hidden.start {
                    let toggle = self.github_fold_toggle(slug, number, Fold::Comments, cx);
                    let text = t_plural(L10nKey::GitHubShowHiddenComments, hidden.len(), &[]);
                    section = section.child(
                        div().pt(px(10.)).px(px(CONTENT_INSET)).child(
                            fold_row("panel-github-fold-comments", None, text, false, cx)
                                .on_click(toggle),
                        ),
                    );
                }
                continue;
            }
            let body = self.github_long_markdown(
                format!("gh-comment-{}-{number}-{}", slug.full(), c.id),
                &c.body,
                "",
                slug,
                number,
                Fold::Comment(c.id),
                cx,
            );
            section = section.child(comment_block(c, now, fg, muted, body));
        }
        if foldable && unfolded {
            let toggle = self.github_fold_toggle(slug, number, Fold::Comments, cx);
            section = section.child(
                div().pt(px(10.)).px(px(CONTENT_INSET)).child(
                    fold_row(
                        "panel-github-fold-comments",
                        None,
                        t(L10nKey::GitHubShowLess).to_string(),
                        true,
                        cx,
                    )
                    .on_click(toggle),
                ),
            );
        }
        if detail.comments_truncated {
            section = section.child(more_on_github(
                "panel-github-more-comments",
                detail.item.html_url.clone(),
                cx,
            ));
        }
        section.into_any_element()
    }

    /// Markdown that is clamped, with a row to unfold it, when its source
    /// runs long.
    #[allow(clippy::too_many_arguments)]
    fn github_long_markdown(
        &self,
        id: String,
        source: &str,
        empty: &str,
        slug: &RepoSlug,
        number: u64,
        fold: Fold,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let block = markdown_block(id.clone(), source, empty, cx);
        if !runs_long(source) {
            return block;
        }
        let unfolded = self.github_unfolded(slug, number, fold);
        let toggle = self.github_fold_toggle(slug, number, fold, cx);
        let text = if unfolded {
            t(L10nKey::GitHubShowLess)
        } else {
            t(L10nKey::GitHubShowFullText)
        };
        v_flex()
            .child(if unfolded {
                block
            } else {
                div()
                    .max_h(rems(CLAMP_HEIGHT))
                    .overflow_hidden()
                    .child(block)
                    .into_any_element()
            })
            .child(
                div().pt(px(4.)).px(px(CONTENT_INSET)).child(
                    fold_row(
                        SharedString::from(format!("{id}-fold")),
                        None,
                        text.to_string(),
                        unfolded,
                        cx,
                    )
                    .on_click(toggle),
                ),
            )
            .into_any_element()
    }

    fn github_detail_files(
        &self,
        repo: &RepoKey,
        slug: &RepoSlug,
        detail: &Detail,
        files: &[PrFile],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let mono = theme.mono_font_family.clone();
        let (added_ink, removed_ink, muted) = (theme.success, theme.danger, theme.muted_foreground);
        let (added, removed) = files
            .iter()
            .fold((0, 0), |(a, r), f| (a + f.additions, r + f.deletions));
        let source = DiffSource::Patch {
            id: format!("{}#{}", slug.full(), detail.item.number),
            label: Some(CommitLabel {
                subject: detail.item.title.clone(),
                author: detail.item.author.clone(),
                at: detail.item.created_at,
            }),
        };
        let head_ref = detail
            .pull
            .as_ref()
            .map(|p| p.head_ref.clone())
            .unwrap_or_default();
        let files: Arc<Vec<PrFile>> = Arc::new(files.to_vec());
        let focused = self
            .diff_overlay_focus(repo.host, &repo.root, &source)
            .map(str::to_string);

        let heading = h_flex()
            .items_center()
            .gap(px(6.))
            .min_h(px(22.))
            .px(px(TEXT_INSET))
            .child(
                div()
                    .text_size(rems(HEADING))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(muted)
                    .child(t_plural(L10nKey::ScmFilesChanged, files.len(), &[])),
            )
            .child(
                h_flex()
                    .gap(px(5.))
                    .text_size(rems(META_MONO))
                    .font_family(mono.clone())
                    .child(div().text_color(added_ink).child(format!("+{added}")))
                    .child(div().text_color(removed_ink).child(format!("−{removed}"))),
            );
        let number = detail.item.number;
        let foldable = files.len() > FILES_FOLD_AT;
        let unfolded = self.github_unfolded(slug, number, Fold::Files);
        // The file open in the diff overlay stays listed, folded or not.
        let shown = |i: usize, f: &PrFile| {
            !foldable
                || unfolded
                || i < FILES_SHOWN_FOLDED
                || focused.as_deref() == Some(f.path.as_str())
        };
        let mut rows = v_flex().px(px(CONTENT_INSET));
        for (i, file) in files.iter().enumerate() {
            if !shown(i, file) {
                continue;
            }
            let selected = focused.as_deref() == Some(file.path.as_str());
            rows = rows.child(
                self.github_file_row(i, file, selected, repo, &source, &head_ref, &files, cx),
            );
        }
        if foldable {
            let text = if unfolded {
                t(L10nKey::GitHubShowLess).to_string()
            } else {
                t_plural(L10nKey::GitHubShowAllFiles, files.len(), &[])
            };
            let toggle = self.github_fold_toggle(slug, number, Fold::Files, cx);
            rows = rows.child(
                fold_row("panel-github-fold-files", None, text, unfolded, cx).on_click(toggle),
            );
        }
        let mut section = v_flex().mt(px(SECTION_GAP)).child(heading).child(rows);
        if detail.files_truncated {
            section = section.child(more_on_github(
                "panel-github-more-files",
                format!("{}/files", detail.item.html_url),
                cx,
            ));
        }
        section.into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn github_file_row(
        &self,
        i: usize,
        file: &PrFile,
        selected: bool,
        repo: &RepoKey,
        source: &DiffSource,
        head_ref: &str,
        files: &Arc<Vec<PrFile>>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let sf = cx.global::<crate::ui::presets::Surfaces>().sidebar;
        let mono = cx.theme().mono_font_family.clone();
        let deco = crate::ui::diff_overlay::deco_status(file.status);
        let (name, dir) = split_display_path(&file.path);
        let (added_ink, removed_ink) = (cx.theme().success, cx.theme().danger);
        let repo = repo.clone();
        let source = source.clone();
        let path = file.path.clone();
        let branch = head_ref.to_string();
        let files = files.clone();
        h_flex()
            .id(("panel-github-file", i))
            .items_center()
            .gap(px(8.))
            .min_h(rems(ROW_H / 16.))
            .w_full()
            .min_w_0()
            .px(px(ROW_INSET))
            .py(px(3.))
            .rounded(ROW_FILL_RADIUS)
            .cursor_pointer()
            .hover(|s| s.bg(gpui::rgb(sf.hover)))
            .when(selected, |s| s.bg(gpui::rgb(sf.selected)))
            .on_click(cx.listener(move |this, _, window, cx| {
                // Built on click, not per frame: a hundred parsed patches is
                // real work, and only the one opened is looked at.
                let snapshot = Arc::new(DiffSnapshot {
                    root: repo.root.clone(),
                    source: source.clone(),
                    branch: branch.clone(),
                    files: files
                        .iter()
                        .map(|f| f.to_file_diff(DiffBudget::default()))
                        .collect(),
                    untracked: Vec::new(),
                    untracked_total: 0,
                    read_failed: false,
                });
                this.open_supplied_diff(repo.host, snapshot, Some(path.clone()), window, cx);
            }))
            .child(git_badge(status_glyph(deco), status_color(deco, cx), &mono))
            .child(
                div()
                    .flex_shrink(1.)
                    .min_w(px(40.))
                    .truncate()
                    .text_size(rems(TEXT))
                    .text_color(if selected {
                        gpui::rgb(sf.text_selected)
                    } else {
                        gpui::rgb(sf.text_resting)
                    })
                    .child(name.to_string()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis_start()
                    .text_right()
                    .text_size(rems(META))
                    .text_color(cx.theme().muted_foreground)
                    .child(dir.to_string()),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap(px(4.))
                    .text_size(rems(META_MONO))
                    .font_family(mono.clone())
                    .when(file.additions > 0, |d| {
                        d.child(
                            div()
                                .text_color(added_ink)
                                .child(format!("+{}", file.additions)),
                        )
                    })
                    .when(file.deletions > 0, |d| {
                        d.child(
                            div()
                                .text_color(removed_ink)
                                .child(format!("−{}", file.deletions)),
                        )
                    }),
            )
            .into_any_element()
    }
}

/// The merge box's one line: a glyph in the colour of how close it is, and
/// the most pressing reason it is not in yet.
fn readiness_line(ready: Readiness, cx: &gpui::App) -> AnyElement {
    let theme = cx.theme();
    let (glyph, ink, text) = match ready {
        Readiness::Ready => (
            CheckState::Passed,
            theme.success,
            t(L10nKey::GitHubReadyToMerge).to_string(),
        ),
        Readiness::Conflicts => (
            CheckState::Failed,
            theme.danger,
            t(L10nKey::GitHubMergeConflicts).to_string(),
        ),
        Readiness::ChecksFailing(n) => (
            CheckState::Failed,
            theme.danger,
            t_plural(L10nKey::GitHubChecksFailing, n, &[]),
        ),
        Readiness::ChangesRequested => (
            CheckState::Failed,
            theme.danger,
            t(L10nKey::GitHubReviewChangesRequested).to_string(),
        ),
        Readiness::WaitingOnChecks(n) => (
            CheckState::Pending,
            theme.warning,
            t_plural(L10nKey::GitHubWaitingOnChecks, n, &[]),
        ),
        Readiness::ReviewRequired => (
            CheckState::Pending,
            theme.warning,
            t(L10nKey::GitHubReviewRequired).to_string(),
        ),
        Readiness::Behind => (
            CheckState::Pending,
            theme.warning,
            t(L10nKey::GitHubBehindBase).to_string(),
        ),
        Readiness::Blocked => (
            CheckState::Failed,
            theme.warning,
            t(L10nKey::GitHubMergeBlocked).to_string(),
        ),
    };
    h_flex()
        .items_center()
        .gap(px(6.))
        .child(check_glyph(glyph, cx))
        .child(
            div()
                .text_size(rems(META))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(ink)
                .child(text),
        )
        .into_any_element()
}

/// `18s`, `1m 12s`, `14m`, `2h 5m` — as short as GitHub's own check list.
pub(crate) fn short_duration(secs: i64) -> String {
    let secs = secs.max(0);
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    match (h, m, s) {
        (0, 0, s) => format!("{s}s"),
        (0, m, 0) => format!("{m}m"),
        (0, m, s) if m < 10 => format!("{m}m {s}s"),
        (0, m, _) => format!("{m}m"),
        (h, 0, _) => format!("{h}h"),
        (h, m, _) => format!("{h}h {m}m"),
    }
}

/// How long a check took, or has been running. Nothing for a check that
/// never said when it started (a commit status, a queued run).
fn check_duration(check: &Check, now: i64) -> Option<String> {
    if check.started_at <= 0 {
        return None;
    }
    let end = match check.state {
        CheckState::Pending => now,
        _ if check.completed_at >= check.started_at => check.completed_at,
        _ => return None,
    };
    Some(short_duration(end - check.started_at))
}

fn checks_section(
    checks: &Checks,
    html_url: &str,
    unfolded: bool,
    toggle: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
    cx: &gpui::App,
) -> AnyElement {
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    let mono = theme.mono_font_family.clone();
    let sf = cx.global::<crate::ui::presets::Surfaces>().sidebar;
    let now = now_unix();
    let counted = checks.counted();
    let summary = if counted == 0 {
        t(L10nKey::GitHubChecksNoneCounted).to_string()
    } else {
        t_fmt(
            L10nKey::GitHubChecksPassed,
            &[
                ("passed", &checks.count(CheckState::Passed).to_string()),
                ("total", &counted.to_string()),
            ],
        )
    };
    let heading = h_flex()
        .items_center()
        .gap(px(6.))
        .min_h(px(22.))
        .px(px(TEXT_INSET))
        .child(
            div()
                .text_size(rems(HEADING))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(muted)
                .child(t(L10nKey::GitHubChecks)),
        )
        .child(div().flex_1())
        .child(div().text_size(rems(META)).text_color(muted).child(summary));
    let foldable = checks.items.len() > CHECKS_FOLD_AT;
    let folded = foldable && !unfolded;
    let quiet = |s: CheckState| matches!(s, CheckState::Passed | CheckState::Skipped);
    let mut rows = v_flex().px(px(CONTENT_INSET));
    for (i, check) in checks.items.iter().enumerate() {
        if folded && quiet(check.state) {
            continue;
        }
        let duration = check_duration(check, now);
        let state = check_label(check.state);
        let mut row = h_flex()
            .id(("panel-github-check", i))
            .items_center()
            .gap(px(8.))
            .h(px(ROW_H))
            .w_full()
            .min_w_0()
            .px(px(ROW_INSET))
            .rounded(ROW_FILL_RADIUS)
            .tooltip(move |window, cx| {
                gpui_component::tooltip::Tooltip::new(state).build(window, cx)
            })
            .child(check_glyph(check.state, cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(rems(TEXT))
                    .text_color(if check.state == CheckState::Skipped {
                        muted
                    } else {
                        gpui::rgb(sf.text_resting).into()
                    })
                    .child(check.name.clone()),
            )
            .children(duration.map(|d| {
                div()
                    .flex_none()
                    .text_size(rems(META_MONO))
                    .font_family(mono.clone())
                    .text_color(muted)
                    .child(d)
            }));
        if let Some(url) = check.url.clone() {
            row = row
                .cursor_pointer()
                .hover(|s| s.bg(gpui::rgb(sf.hover)))
                .on_click(move |_, _window, cx| cx.open_url(&url));
        }
        rows = rows.child(row);
    }
    if foldable {
        let (passed, skipped) = (
            checks.count(CheckState::Passed),
            checks.count(CheckState::Skipped),
        );
        let (glyph, text) = if unfolded {
            (None, t(L10nKey::GitHubShowLess).to_string())
        } else {
            let mut parts = Vec::new();
            if passed > 0 {
                parts.push(t_plural(L10nKey::GitHubPassedCount, passed, &[]));
            }
            if skipped > 0 {
                parts.push(t_plural(L10nKey::GitHubSkippedCount, skipped, &[]));
            }
            let glyph = if passed > 0 {
                CheckState::Passed
            } else {
                CheckState::Skipped
            };
            (Some(glyph), parts.join(" · "))
        };
        rows = rows.child(
            fold_row("panel-github-fold-checks", glyph, text, unfolded, cx).on_click(toggle),
        );
    }
    let mut section = v_flex().mt(px(SECTION_GAP)).child(heading).child(rows);
    if checks.truncated {
        section = section.child(more_on_github(
            "panel-github-more-checks",
            format!("{html_url}/checks"),
            cx,
        ));
    }
    section.into_any_element()
}

/// The row a folded section ends in: what is tucked away (or "show less"),
/// and a chevron saying which way it goes.
fn fold_row(
    id: impl Into<gpui::ElementId>,
    glyph: Option<CheckState>,
    text: String,
    unfolded: bool,
    cx: &gpui::App,
) -> gpui::Stateful<gpui::Div> {
    let sf = cx.global::<crate::ui::presets::Surfaces>().sidebar;
    let muted = cx.theme().muted_foreground;
    h_flex()
        .id(id)
        .items_center()
        .gap(px(8.))
        .h(px(ROW_H))
        .w_full()
        .min_w_0()
        .px(px(ROW_INSET))
        .rounded(ROW_FILL_RADIUS)
        .cursor_pointer()
        .hover(|s| s.bg(gpui::rgb(sf.hover)))
        .children(glyph.map(|g| check_glyph(g, cx)))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(rems(META))
                .text_color(muted)
                .child(text),
        )
        .child(
            Icon::new(if unfolded {
                IconName::ChevronUp
            } else {
                IconName::ChevronDown
            })
            .xsmall()
            .text_color(muted),
        )
}

fn reviews_section(
    reviewers: &[Reviewer],
    unfolded: bool,
    toggle: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
    cx: &gpui::App,
) -> AnyElement {
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    let sf = cx.global::<crate::ui::presets::Surfaces>().sidebar;
    let foldable = reviewers.len() > REVIEWERS_FOLD_AT;
    let shown = if foldable && !unfolded {
        REVIEWERS_FOLD_AT - 1
    } else {
        reviewers.len()
    };
    let mut rows = v_flex().px(px(CONTENT_INSET));
    for (i, r) in reviewers.iter().enumerate().take(shown) {
        let (label, ink) = match r.state {
            ReviewState::Approved => (L10nKey::GitHubReviewApproved, theme.success),
            ReviewState::ChangesRequested => (L10nKey::GitHubReviewChangesRequested, theme.danger),
            ReviewState::Commented => (L10nKey::GitHubReviewCommented, muted),
            ReviewState::Requested => (L10nKey::GitHubReviewRequested, muted),
        };
        let initial = r
            .login
            .chars()
            .next()
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or_default();
        rows = rows.child(
            h_flex()
                .id(("panel-github-reviewer", i))
                .items_center()
                .gap(px(8.))
                .h(px(ROW_H))
                .w_full()
                .min_w_0()
                .px(px(ROW_INSET))
                .child(
                    div()
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(16.))
                        .rounded_full()
                        .bg(theme.foreground.opacity(0.08))
                        .text_size(rems(10. / 16.))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(muted)
                        .child(initial),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(rems(TEXT))
                        .text_color(gpui::rgb(sf.text_resting))
                        .child(r.login.clone()),
                )
                .child(
                    div()
                        .flex_none()
                        .text_size(rems(META))
                        .text_color(ink)
                        .child(t(label)),
                ),
        );
    }
    if foldable {
        let text = if unfolded {
            t(L10nKey::GitHubShowLess).to_string()
        } else {
            t_plural(L10nKey::GitHubShowAllReviewers, reviewers.len(), &[])
        };
        rows = rows.child(
            fold_row("panel-github-fold-reviews", None, text, unfolded, cx).on_click(toggle),
        );
    }
    v_flex()
        .mt(px(SECTION_GAP))
        .child(section_heading(t(L10nKey::GitHubReviews).to_string(), cx))
        .child(rows)
        .into_any_element()
}

/// Headings a step or two over the body rather than the document-sized ramp
/// the text view defaults to: an issue's `## What happened?` is a label in a
/// 280px column, not a page title.
fn panel_markdown_style(cx: &gpui::App) -> gpui_component::text::TextViewStyle {
    gpui_component::text::TextViewStyle {
        heading_font_size: Some(Arc::new(|level, base| match level {
            1 => base * 1.25,
            2 => base * 1.15,
            _ => base * 1.05,
        })),
        ..crate::ui::theme::markdown_style(cx)
    }
}

fn section_heading(text: String, cx: &gpui::App) -> AnyElement {
    div()
        .flex()
        .items_center()
        .min_h(px(22.))
        .px(px(TEXT_INSET))
        .text_size(rems(HEADING))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(cx.theme().muted_foreground)
        .child(text)
        .into_any_element()
}

/// Untrusted Markdown, made safe and rendered. An empty body says so.
fn markdown_block(id: String, source: &str, empty: &str, cx: &gpui::App) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    if source.trim().is_empty() {
        return div()
            .px(px(TEXT_INSET))
            .text_size(rems(META))
            .text_color(muted)
            .italic()
            .child(empty.to_string())
            .into_any_element();
    }
    let safe = tty7_core::core::github::markdown::sanitize(source, t(L10nKey::GitHubImage));
    div()
        .px(px(TEXT_INSET))
        .min_w_0()
        .text_size(rems(TEXT))
        .child(
            gpui_component::text::TextView::markdown(SharedString::from(id), safe)
                .style(panel_markdown_style(cx))
                .selectable(true)
                // The rewrite above works on lines of source; this checks what
                // the parser actually made of them. A block holding anything
                // the rewrite should have removed is drawn as its plain source
                // instead, so a construct the two read differently can never
                // load a third-party image or hand the OS a `file:` link.
                .markdown_block_parser(|node, parse| {
                    (!renders_safely(node)).then(|| {
                        let text = parse.node_source(node).unwrap_or_default().to_string();
                        gpui_component::text::MarkdownNode::new(UNSAFE_BLOCK, text.clone())
                            .text(text)
                    })
                })
                .markdown_block_renderer(UNSAFE_BLOCK, |node, _window, cx| {
                    div()
                        .text_color(cx.theme().muted_foreground)
                        .whitespace_normal()
                        .child(node.data::<String>().cloned().unwrap_or_default())
                }),
        )
        .into_any_element()
}

/// The custom block a Markdown block that failed [`renders_safely`] becomes.
const UNSAFE_BLOCK: &str = "tty7-github-unsafe-block";

/// Whether `node` and everything under it is what
/// `core::github::markdown::sanitize` promises: images only from GitHub's own
/// hosts, links (and link definitions) only to web and mail targets, and raw
/// HTML with no `<img>` or unsafe `href`/`src`.
fn renders_safely(node: &gpui_component::text::markdown_ast::Node) -> bool {
    use gpui_component::text::markdown_ast::Node;
    use tty7_core::core::github::markdown::{html_is_safe, is_github_hosted, is_safe_target};
    let here = match node {
        Node::Image(image) => is_github_hosted(&image.url),
        // Never written by the rewrite (it makes every `![…][…]` a link).
        Node::ImageReference(_) => false,
        Node::Link(link) => is_safe_target(&link.url),
        Node::Definition(def) => is_safe_target(&def.url),
        Node::Html(html) => html_is_safe(&html.value),
        _ => true,
    };
    here && node
        .children()
        .is_none_or(|children| children.iter().all(renders_safely))
}

fn comment_block(
    c: &Comment,
    now: i64,
    fg: gpui::Hsla,
    muted: gpui::Hsla,
    body: AnyElement,
) -> AnyElement {
    let when = (c.created_at > 0).then(|| relative_time(now, c.created_at));
    v_flex()
        .pt(px(14.))
        .gap(px(4.))
        .child(
            h_flex()
                .px(px(TEXT_INSET))
                .gap(px(6.))
                .items_baseline()
                .text_size(rems(META))
                .child(
                    div()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(fg)
                        .child(c.author.clone()),
                )
                .children(when.map(|w| div().text_color(muted).child(w))),
        )
        .child(body)
        .into_any_element()
}

/// Whether a Markdown source is long enough to clamp.
fn runs_long(source: &str) -> bool {
    source.chars().count() > CLAMP_CHARS || source.lines().count() > CLAMP_LINES
}

fn more_on_github(id: &'static str, url: String, cx: &gpui::App) -> AnyElement {
    h_flex()
        .id(id)
        .mt(px(6.))
        .mx(px(TEXT_INSET))
        .gap(px(4.))
        .items_center()
        .cursor_pointer()
        .text_size(rems(META))
        .text_color(cx.theme().link)
        .hover(|s| s.underline())
        .on_click(move |_, _window, cx| cx.open_url(&url))
        .child(t(L10nKey::GitHubMoreOnGitHub))
        .child(Icon::new(IconName::ExternalLink).xsmall())
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::{runs_long, short_duration};

    #[test]
    fn a_pasted_log_runs_long_and_a_few_paragraphs_do_not() {
        assert!(!runs_long("It crashes.\n\nSteps:\n1. open\n2. resize"));
        assert!(runs_long(&"line\n".repeat(40)));
        assert!(runs_long(&"word ".repeat(400)));
    }

    #[test]
    fn durations_read_as_short_as_githubs() {
        assert_eq!(short_duration(18), "18s");
        assert_eq!(short_duration(120), "2m");
        assert_eq!(short_duration(72), "1m 12s");
        assert_eq!(short_duration(14 * 60 + 5), "14m");
        assert_eq!(short_duration(3600), "1h");
        assert_eq!(short_duration(2 * 3600 + 5 * 60), "2h 5m");
        assert_eq!(
            short_duration(-3),
            "0s",
            "a clock skew is not negative time"
        );
    }
}

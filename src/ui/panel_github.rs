//! The right panel's GitHub tab: the list of issues or pull requests.
//!
//! State and fetching live in [`crate::ui::github`]; the single-item view in
//! [`crate::ui::github::detail`]. This file draws the chrome over the list —
//! which repository, which kind, which state, which label — and the rows.

use gpui::{AnyElement, Context, SharedString, Window, div, prelude::*, px, relative, rems};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};

use tty7_core::core::github::{
    ApiError, GitHubRemote, Item, ItemState, Kind, Label, RepoSlug, StateFilter,
};

use crate::ui::app::{CONTENT_INSET, TILE_GLYPH_XS, TILE_SIZE_XS, Tty7App};
use crate::ui::github::{GhTarget, now_unix};
use crate::ui::i18n::{L10nKey, t, t_fmt};
use crate::ui::right_panel::{META, ROW_FILL_RADIUS, ROW_INSET, TAB_TEXT, TEXT, TEXT_INSET};
use crate::ui::scm::path::relative_time;
use crate::ui::scm::state::RepoKey;

/// A list row: one line, the Source Control tab's row pitch.
const ROW_H: f32 = 26.;
/// How many label chips a hovered row shows before the age.
const MAX_ROW_LABELS: usize = 3;
/// The state glyph, and the column it sits in.
const GLYPH: f32 = 14.;
/// The pinned rows' height — the Info tab's row pitch.
const PINNED_ROW_H: f32 = 28.;
/// The segmented switches' cells.
const SWITCH_H: f32 = 22.;
/// The pinned header borrows the Git tab's grammar for its own pinned block:
/// 10px between its parts, 14px before the list, and a side inset 2px further
/// in than the list's, so the switches read as controls rather than rows.
const PINNED_GAP: f32 = 10.;
const LIST_GAP: f32 = 14.;
const PINNED_INSET: f32 = 14.;

impl Tty7App {
    pub(crate) fn render_panel_github(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let target = self.github_target(window, cx);
        // No trailing tile: on macOS that would add a "GitHub" heading row no
        // other tab has, just to hold ↻. Refresh sits with the repository's
        // other actions instead — in the repo row, and in a detail's header.
        let title = self.panel_title(t(L10nKey::PanelGitHubTitle), None, None, window, cx);
        let (repo, remotes, chosen) = match target {
            GhTarget::NoPane => {
                let body = self.panel_empty(
                    t(L10nKey::PanelNoWorkingDirectory),
                    Some(t(L10nKey::PanelNoWorkingDirectoryHint)),
                    cx,
                );
                return self.github_shell(title, Vec::new(), body, false);
            }
            GhTarget::Pending => {
                let body = self.panel_empty(t(L10nKey::PanelLoading), None, cx);
                return self.github_shell(title, Vec::new(), body, false);
            }
            GhTarget::NotARepo => {
                let body = self.panel_empty(
                    t(L10nKey::PanelNotAGitRepo),
                    Some(t(L10nKey::PanelNotAGitRepoHint)),
                    cx,
                );
                return self.github_shell(title, Vec::new(), body, false);
            }
            GhTarget::NoRemote => {
                let body = self.panel_empty(
                    t(L10nKey::GitHubNoRemote),
                    Some(t(L10nKey::GitHubNoRemoteHint)),
                    cx,
                );
                return self.github_shell(title, Vec::new(), body, false);
            }
            GhTarget::Ready {
                repo,
                remotes,
                chosen,
            } => (repo, remotes, chosen),
        };

        // A detail belongs to the repository it was opened from. If the pane
        // has since moved to another one, the list of that one is what the
        // panel is about now.
        if let Some((slug, number)) = self.github.open.clone() {
            if slug == chosen.slug {
                let (pinned, body) = self.render_github_detail(&repo, &slug, number, window, cx);
                return self.github_shell(title, pinned, body, true);
            }
            self.github.open = None;
        }

        let query = self.github_query(&chosen.slug);
        self.github_ensure_list(&query, cx);

        let mut pinned = vec![
            self.github_repo_row(&repo, &remotes, &chosen, cx),
            self.github_switch_row(cx),
        ];
        if let Some(label) = self.github.label.clone() {
            pinned.push(self.github_label_filter_row(&label, cx));
        }
        // The Git tab's gap under its pinned block, so the header reads as
        // one unit and the list starts clear of it.
        pinned.push(div().flex_none().h(px(LIST_GAP)).into_any_element());
        let body = self.github_list_body(&chosen.slug, cx);
        self.github_shell(title, pinned, body, false)
    }

    /// Title, the rows pinned under it, and a body that scrolls on its own.
    fn github_shell(
        &self,
        title: AnyElement,
        pinned: Vec<AnyElement>,
        body: AnyElement,
        detail: bool,
    ) -> AnyElement {
        let scroll = if detail {
            &self.github.detail_scroll
        } else {
            &self.github.list_scroll
        };
        let scroller = div()
            .id(if detail {
                "panel-github-detail"
            } else {
                "panel-github-list"
            })
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(scroll)
            .child(body);
        v_flex()
            .flex_1()
            .min_h_0()
            .child(title)
            .children(pinned)
            .child(crate::ui::scrollbar::with_vertical_scrollbar(
                "panel-github-scrollbar",
                scroller,
                scroll,
            ))
            .into_any_element()
    }

    pub(crate) fn github_refresh_tile(
        &self,
        repo: Option<RepoKey>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let busy = self.github.connecting
            || self.github.lists.values().any(|l| l.loading)
            || self.github.details.values().any(|d| d.loading);
        github_tile(
            "panel-github-refresh",
            if busy {
                Icon::new(IconName::LoaderCircle)
            } else {
                Icon::empty().path("icons/refresh.svg")
            },
            t(L10nKey::GitHubRefresh),
            cx,
        )
        .on_click(cx.listener(move |this, _, _window, cx| {
            this.github_refresh(repo.clone(), cx);
        }))
        .into_any_element()
    }

    /// `owner/name` with the remote it came from, a picker when there are
    /// several, and a way out to the browser.
    fn github_repo_row(
        &self,
        repo: &RepoKey,
        remotes: &[GitHubRemote],
        chosen: &GitHubRemote,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let fg = cx.theme().foreground;
        let url = chosen.slug.web_url();
        let name = h_flex()
            .flex_1()
            .min_w_0()
            .items_baseline()
            .gap(px(6.))
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(fg)
                    .child(chosen.slug.full()),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(rems(META))
                    .text_color(muted)
                    .child(chosen.remote.clone()),
            );
        let picker: AnyElement = if remotes.len() > 1 {
            let app = cx.entity().downgrade();
            let repo = repo.clone();
            let remotes: Vec<GitHubRemote> = remotes.to_vec();
            let current = chosen.remote.clone();
            div()
                .flex_1()
                .min_w_0()
                .ml(px(-6.))
                .child(
                    Button::new("panel-github-remote")
                        .ghost()
                        .small()
                        .dropdown_caret(true)
                        .child(name.line_height(relative(1.)))
                        .w_full()
                        .h(rems(24. / 16.))
                        .pl(px(6.))
                        .rounded(px(5.))
                        .dropdown_menu_with_anchor(
                            gpui::Anchor::TopLeft,
                            move |menu, _window, _cx| {
                                let mut menu = menu
                                    .min_w(px(200.))
                                    .item(PopupMenuItem::label(t(L10nKey::GitHubShowRemote)));
                                for r in &remotes {
                                    let label = format!("{}  {}", r.remote, r.slug.full());
                                    menu = menu.item(
                                        PopupMenuItem::new(label)
                                            .checked(r.remote == current)
                                            .on_click({
                                                let app = app.clone();
                                                let repo = repo.clone();
                                                let name = r.remote.clone();
                                                move |_, _window, cx| {
                                                    let _ = app.update(cx, |this, cx| {
                                                        this.github
                                                            .remote_pick
                                                            .insert(repo.clone(), name.clone());
                                                        this.github.open = None;
                                                        cx.notify();
                                                    });
                                                }
                                            }),
                                    );
                                }
                                menu
                            },
                        ),
                )
                .into_any_element()
        } else {
            name.into_any_element()
        };
        h_flex()
            .flex_none()
            .items_center()
            .gap(px(6.))
            .min_h(px(PINNED_ROW_H))
            .pl(px(TEXT_INSET))
            .pr(px(CONTENT_INSET))
            .text_size(rems(TEXT))
            .child(
                Icon::empty()
                    .path("icons/github.svg")
                    .xsmall()
                    .text_color(muted),
            )
            .child(picker)
            .child(self.github_refresh_tile(Some(repo.clone()), cx))
            .child(
                github_tile(
                    "panel-github-open-repo",
                    Icon::new(IconName::ExternalLink),
                    t(L10nKey::GitHubOpenOnGitHub),
                    cx,
                )
                .on_click(move |_, _window, cx| cx.open_url(&url)),
            )
            .into_any_element()
    }

    /// Issues | Pull Requests on the left, Open | Closed on the right.
    fn github_switch_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let kind = self.github.kind;
        let state = self.github.state;
        let kinds = [
            (Kind::Issues, t(L10nKey::GitHubIssues)),
            (Kind::Pulls, t(L10nKey::GitHubPulls)),
        ];
        let states = [
            (StateFilter::Open, t(L10nKey::GitHubOpen)),
            (StateFilter::Closed, t(L10nKey::GitHubClosed)),
        ];
        let kind_cells = kinds.into_iter().enumerate().map(|(i, (k, label))| {
            switch_cell(("panel-github-kind", i), label, k == kind, cx)
                .on_click(cx.listener(move |this, _, _window, cx| {
                    this.github.kind = k;
                    this.github.list_scroll = gpui::ScrollHandle::new();
                    cx.notify();
                }))
                .into_any_element()
        });
        let state_cells = states.into_iter().enumerate().map(|(i, (s, label))| {
            switch_cell(("panel-github-state", i), label, s == state, cx)
                .on_click(cx.listener(move |this, _, _window, cx| {
                    this.github.state = s;
                    this.github.list_scroll = gpui::ScrollHandle::new();
                    cx.notify();
                }))
                .into_any_element()
        });
        h_flex()
            .flex_none()
            .items_center()
            .justify_between()
            .flex_wrap()
            // Wrapped onto two lines in a narrow panel, the pairs sit as far
            // apart as the header's parts do, or the two selected pills stack
            // into one lump.
            .gap_x(px(8.))
            .gap_y(px(PINNED_GAP))
            .px(px(PINNED_INSET))
            .pt(px(PINNED_GAP))
            .child(h_flex().gap(px(2.)).children(kind_cells))
            .child(h_flex().gap(px(2.)).children(state_cells))
            .into_any_element()
    }

    fn github_label_filter_row(&self, label: &str, cx: &mut Context<Self>) -> AnyElement {
        let known = self
            .github
            .lists
            .values()
            .flat_map(|l| l.items.iter())
            .flat_map(|i| i.labels.iter())
            .find(|l| l.name == label)
            .cloned()
            .unwrap_or(Label {
                name: label.to_string(),
                color: None,
            });
        h_flex()
            .flex_none()
            .items_center()
            .gap(px(4.))
            .min_h(px(PINNED_ROW_H - 4.))
            .px(px(PINNED_INSET))
            .pt(px(PINNED_GAP))
            .child(label_chip(&known, cx))
            .child(
                github_tile(
                    "panel-github-clear-label",
                    Icon::new(IconName::Close),
                    t(L10nKey::GitHubClearLabel),
                    cx,
                )
                .on_click(cx.listener(|this, _, _window, cx| {
                    this.github.label = None;
                    cx.notify();
                })),
            )
            .into_any_element()
    }

    fn github_list_body(&mut self, slug: &RepoSlug, cx: &mut Context<Self>) -> AnyElement {
        let query = self.github_query(slug);
        let authenticated = self.github_authenticated();
        let Some(cache) = self.github.lists.get(&query) else {
            return self.panel_empty(t(L10nKey::PanelLoading), None, cx);
        };
        let items = cache.items.clone();
        let error = cache.error.clone();
        let next_page = cache.next_page;
        let loading = cache.loading;
        let loaded = cache.loaded;

        let mut body = v_flex().pb(px(12.));
        if let Some(err) = &error {
            let (text, hint) = describe_error(err, authenticated);
            body = body.child(self.panel_empty(&text, hint.as_deref(), cx));
        }
        // An empty page with more behind it is not "no issues": `/issues`
        // pages filtered to one kind can come back empty (see `api::list`).
        if items.is_empty() && next_page.is_none() {
            if error.is_none() {
                let text = if loaded && !loading {
                    match query.kind {
                        Kind::Issues => t(L10nKey::GitHubNoIssues),
                        Kind::Pulls => t(L10nKey::GitHubNoPulls),
                    }
                } else {
                    t(L10nKey::PanelLoading)
                };
                body = body.child(self.panel_empty(text, None, cx));
            }
            return body.into_any_element();
        }
        let now = now_unix();
        let mut rows = v_flex().px(px(CONTENT_INSET));
        for item in items.iter() {
            rows = rows.child(self.github_item_row(slug, item, now, cx));
        }
        body = body.child(rows);
        if let Some(page) = next_page {
            let q = query.clone();
            body = body.child(
                h_flex().px(px(CONTENT_INSET)).pt(px(4.)).child(
                    Button::new("panel-github-more")
                        .ghost()
                        .small()
                        .w_full()
                        .loading(loading)
                        .label(t(L10nKey::GitHubLoadMore))
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            this.github_fetch_list(q.clone(), page, cx);
                        })),
                ),
            );
        }
        body.into_any_element()
    }

    /// One line per item: state glyph, `#number`, title. The labels and the
    /// age of the last update wait for the pointer — the resting list reads
    /// as a column of titles, and hovering a row answers "what labels, how
    /// fresh" without a second line under every one of them.
    fn github_item_row(
        &self,
        slug: &RepoSlug,
        item: &Item,
        now: i64,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let sf = cx.global::<crate::ui::presets::Surfaces>().sidebar;
        let muted = cx.theme().muted_foreground;
        let mono = cx.theme().mono_font_family.clone();
        let number = item.number;
        let slug = slug.clone();
        let hovered = self.github.hovered == Some(number);
        let title = SharedString::from(item.title.clone());
        let labels = item
            .labels
            .iter()
            .take(MAX_ROW_LABELS)
            .enumerate()
            .map(|(i, label)| {
                let name = label.name.clone();
                div()
                    .id(SharedString::from(format!(
                        "panel-github-label-{number}-{i}"
                    )))
                    .flex_none()
                    .cursor_pointer()
                    .tooltip(|window, cx| {
                        gpui_component::tooltip::Tooltip::new(t(L10nKey::GitHubFilterByLabel))
                            .build(window, cx)
                    })
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        cx.stop_propagation();
                        this.github.label = Some(name.clone());
                        this.github.list_scroll = gpui::ScrollHandle::new();
                        cx.notify();
                    }))
                    .child(label_chip(label, cx))
            });
        let age = (item.updated_at > 0).then(|| relative_time(now, item.updated_at));
        // Out of the layout at rest, so the title has the whole row to
        // itself; on hover it takes its room from the title's tail. Built
        // from state rather than a `group_hover` display switch: an element
        // that is `display: none` at layout is never prepainted, and gpui
        // panics when a hover style then asks to paint it.
        let meta = hovered.then(|| {
            h_flex()
                .flex_none()
                .items_center()
                .gap(px(4.))
                .children(labels)
                .children(age.map(|age| {
                    div()
                        .flex_none()
                        .text_size(rems(META))
                        .text_color(muted)
                        .child(age)
                }))
        });
        h_flex()
            .id(SharedString::from(format!("panel-github-item-{number}")))
            .items_center()
            .gap(px(8.))
            .h(px(ROW_H))
            .w_full()
            .px(px(ROW_INSET))
            .rounded(ROW_FILL_RADIUS)
            .cursor_pointer()
            .hover(|s| s.bg(gpui::rgb(sf.hover)))
            .tooltip(move |window, cx| {
                gpui_component::tooltip::Tooltip::new(title.clone()).build(window, cx)
            })
            .on_hover(cx.listener(move |this, over: &bool, _window, cx| {
                let next = match (*over, this.github.hovered) {
                    (true, _) => Some(number),
                    (false, Some(n)) if n == number => None,
                    (false, other) => other,
                };
                if next != this.github.hovered {
                    this.github.hovered = next;
                    cx.notify();
                }
            }))
            .on_click(cx.listener(move |this, _, _window, cx| {
                this.github_open_detail(slug.clone(), number, cx);
            }))
            .child(
                div()
                    .flex_none()
                    .child(state_glyph(item.state, item.is_pr, cx)),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(rems(META))
                    .font_family(mono)
                    .text_color(muted)
                    .child(format!("#{number}")),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(rems(TEXT))
                    .text_color(gpui::rgb(sf.text_resting))
                    .child(item.title.clone()),
            )
            .children(meta)
            .into_any_element()
    }

    /// Whether requests go out signed in. Unknown (still connecting) reads as
    /// signed in, so no sign-in advice flashes up while the token resolves.
    pub(crate) fn github_authenticated(&self) -> bool {
        self.github
            .connection
            .as_ref()
            .is_none_or(|c| c.transport.authenticated())
    }
}

/// A 18px chrome tile, the Info rows' size.
pub(crate) fn github_tile(
    id: &'static str,
    icon: Icon,
    tooltip: &'static str,
    cx: &gpui::App,
) -> Button {
    crate::ui::tab_strip::chrome_tile_sized(
        Button::new(id).icon(icon),
        TILE_SIZE_XS,
        TILE_GLYPH_XS,
        false,
        cx,
    )
    .rounded(px(4.))
    .tooltip(tooltip)
}

fn switch_cell(
    id: (&'static str, usize),
    label: &'static str,
    live: bool,
    cx: &gpui::App,
) -> gpui::Stateful<gpui::Div> {
    let sf = cx.global::<crate::ui::presets::Surfaces>().sidebar;
    h_flex()
        .id(id)
        .flex_none()
        .items_center()
        .h(px(SWITCH_H))
        .px(px(8.))
        .rounded(px(6.))
        .text_size(rems(TAB_TEXT))
        .whitespace_nowrap()
        .cursor_pointer()
        .when(live, |cell| {
            cell.bg(gpui::rgb(sf.selected))
                .text_color(gpui::rgb(sf.text_selected))
                .font_weight(gpui::FontWeight::MEDIUM)
        })
        .when(!live, |cell| {
            cell.text_color(cx.theme().muted_foreground)
                .hover(|h| h.bg(gpui::rgb(sf.hover)))
        })
        .active(|cell| cell.bg(gpui::rgb(sf.pressed)))
        .child(label)
}

/// The glyph for a state: a distinct *shape* for each (open, closed, not
/// planned, merged, draft), then GitHub's own colour family on top — green
/// open, purple done, red closed-unmerged, grey not planned or draft.
pub(crate) fn state_glyph(state: ItemState, is_pr: bool, cx: &gpui::App) -> AnyElement {
    let theme = cx.theme();
    let purple = gpui::hsla(270. / 360., 0.55, 0.6, 1.);
    let (path, color) = match (state, is_pr) {
        (ItemState::Open, false) => ("icons/github/issue-open.svg", theme.success),
        (ItemState::Closed, false) => ("icons/github/issue-closed.svg", purple),
        (ItemState::NotPlanned, _) => {
            ("icons/github/issue-not-planned.svg", theme.muted_foreground)
        }
        (ItemState::Draft, _) => ("icons/github/pr-draft.svg", theme.muted_foreground),
        (ItemState::Open, true) => ("icons/github/pr-open.svg", theme.success),
        (ItemState::Closed, true) => ("icons/github/pr-closed.svg", theme.danger),
        (ItemState::Merged, _) => ("icons/github/pr-merged.svg", purple),
    };
    gpui::svg()
        .path(path)
        .flex_none()
        .size(px(GLYPH))
        .text_color(color)
        .into_any_element()
}

pub(crate) fn state_label(state: ItemState) -> &'static str {
    t(match state {
        ItemState::Open => L10nKey::GitHubOpen,
        ItemState::Draft => L10nKey::GitHubDraft,
        ItemState::Closed => L10nKey::GitHubClosed,
        ItemState::NotPlanned => L10nKey::GitHubNotPlanned,
        ItemState::Merged => L10nKey::GitHubMerged,
    })
}

/// A label in GitHub's colour: a faint fill of it, and the ink of it pulled
/// toward the theme's foreground so a pale yellow label stays readable.
pub(crate) fn label_chip(label: &Label, cx: &gpui::App) -> AnyElement {
    let theme = cx.theme();
    let (fill, ink) = match label.color {
        Some(rgb) => {
            let c: gpui::Hsla = gpui::rgb(rgb).into();
            let l = if theme.mode.is_dark() {
                c.l.max(0.65)
            } else {
                c.l.min(0.38)
            };
            (c.opacity(0.18), gpui::hsla(c.h, c.s.min(0.8), l, 1.))
        }
        None => (theme.foreground.opacity(0.08), theme.muted_foreground),
    };
    div()
        .flex_none()
        .px(px(6.))
        .py(px(0.5))
        .rounded(px(8.))
        .bg(fill)
        .text_size(rems(META))
        .text_color(ink)
        .whitespace_nowrap()
        .child(label.name.clone())
        .into_any_element()
}

/// What to tell the user about a failed call, and what they can do about it.
pub(crate) fn describe_error(err: &ApiError, authenticated: bool) -> (String, Option<String>) {
    let sign_in = || Some(t(L10nKey::GitHubSignInHint).to_string());
    match err {
        ApiError::NotFound if authenticated => (t(L10nKey::GitHubNotFoundSignedIn).into(), None),
        ApiError::NotFound => (t(L10nKey::GitHubNotFoundSignedOut).into(), sign_in()),
        ApiError::Unauthorized => (t(L10nKey::GitHubUnauthorized).into(), sign_in()),
        ApiError::RateLimited { reset } => {
            let mut hint: Vec<String> = Vec::new();
            if let Some(at) = reset {
                let minutes = ((at - now_unix()).max(0) + 59) / 60;
                hint.push(t_fmt(
                    L10nKey::GitHubRateLimitResetIn,
                    &[("n", &minutes.max(1).to_string())],
                ));
            }
            if !authenticated {
                hint.push(t(L10nKey::GitHubRateLimitSignedOut).to_string());
            }
            (
                t(L10nKey::GitHubRateLimited).into(),
                (!hint.is_empty()).then(|| hint.join(" ")),
            )
        }
        ApiError::Forbidden(msg) => (
            t(L10nKey::GitHubForbidden).into(),
            (!msg.is_empty()).then(|| msg.clone()),
        ),
        ApiError::Network(msg) => (t(L10nKey::GitHubNetworkError).into(), Some(msg.clone())),
        ApiError::Http(code) => (
            t_fmt(L10nKey::GitHubHttpError, &[("code", &code.to_string())]),
            None,
        ),
        ApiError::Decode(_) => (t(L10nKey::GitHubDecodeError).into(), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_signed_out_rate_limit_says_when_and_how_to_raise_it() {
        crate::ui::i18n::set_locale("en");
        let reset = now_unix() + 10 * 60;
        let (text, hint) = describe_error(&ApiError::RateLimited { reset: Some(reset) }, false);
        assert_eq!(text, t(L10nKey::GitHubRateLimited));
        let hint = hint.expect("a rate limit explains itself");
        assert!(hint.contains("10 min"), "{hint}");
        assert!(hint.contains("gh auth login"), "{hint}");

        let (_, hint) = describe_error(&ApiError::RateLimited { reset: None }, true);
        assert!(
            hint.is_none(),
            "signed in, with no reset time, there is nothing to add"
        );
    }

    #[test]
    fn a_404_is_explained_by_whether_the_request_was_signed_in() {
        crate::ui::i18n::set_locale("en");
        let (out, hint) = describe_error(&ApiError::NotFound, false);
        assert_eq!(out, t(L10nKey::GitHubNotFoundSignedOut));
        assert!(hint.unwrap().contains("gh auth login"));
        let (inn, hint) = describe_error(&ApiError::NotFound, true);
        assert_eq!(inn, t(L10nKey::GitHubNotFoundSignedIn));
        assert!(hint.is_none());
        let (_, hint) = describe_error(&ApiError::Unauthorized, true);
        assert!(hint.unwrap().contains("gh auth login"));
    }

    #[test]
    fn every_error_has_something_to_say() {
        crate::ui::i18n::set_locale("en");
        for err in [
            ApiError::Unauthorized,
            ApiError::NotFound,
            ApiError::RateLimited { reset: None },
            ApiError::Forbidden(String::new()),
            ApiError::Network("dns".into()),
            ApiError::Http(502),
            ApiError::Decode("eof".into()),
        ] {
            let (text, _) = describe_error(&err, true);
            assert!(!text.is_empty(), "{err:?}");
        }
        assert!(describe_error(&ApiError::Http(502), true).0.contains("502"));
    }
}

/// The tab end to end against a real repository and a fixture transport: the
/// remote is read through the host, the list and a detail are fetched, and
/// nothing reaches the network.
#[cfg(test)]
mod gpui_tests {
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    use gpui::{Entity, TestAppContext, VisualTestContext};
    use tty7_core::core::config::RightPanelTab;
    use tty7_core::core::github::{ApiError, Kind, Reply, Transport};

    use crate::daemon::protocol::DaemonMsg;
    use crate::ui::app::{Tty7App, test_window};
    use crate::ui::github::Connection;

    struct Fake {
        asked: Mutex<Vec<String>>,
    }

    impl Transport for Fake {
        fn get(&self, path: &str) -> Result<Reply, ApiError> {
            self.asked.lock().unwrap().push(path.to_string());
            let body = if path.starts_with("/repos/acme/widgets/issues?") {
                r#"[{"number": 7, "title": "Widgets wobble", "state": "open",
                     "user": {"login": "ada"}, "labels": [{"name": "bug", "color": "d73a4a"}],
                     "comments": 1, "created_at": "2026-09-01T10:00:00Z",
                     "updated_at": "2026-09-02T10:00:00Z",
                     "html_url": "https://github.com/acme/widgets/issues/7"},
                    {"number": 8, "title": "A pull request", "state": "open",
                     "pull_request": {"merged_at": null},
                     "created_at": "2026-09-01T10:00:00Z", "updated_at": "2026-09-02T10:00:00Z",
                     "html_url": "https://github.com/acme/widgets/pull/8"}]"#
            } else if path == "/repos/acme/widgets/issues/7" {
                r#"{"number": 7, "title": "Widgets wobble", "state": "open",
                    "user": {"login": "ada"}, "labels": [], "comments": 1,
                    "created_at": "2026-09-01T10:00:00Z", "updated_at": "2026-09-02T10:00:00Z",
                    "html_url": "https://github.com/acme/widgets/issues/7",
                    "body": "They **wobble**. ![shot](https://example.com/x.png)"}"#
            } else if path.starts_with("/repos/acme/widgets/issues/7/comments") {
                r#"[{"id": 1, "user": {"login": "bob"}, "body": "Seen it too.",
                     "created_at": "2026-09-02T10:00:00Z", "html_url": "https://x/1"}]"#
            } else {
                return Err(ApiError::NotFound);
            };
            Ok(Reply {
                body: body.as_bytes().to_vec(),
                has_next: false,
            })
        }

        fn authenticated(&self) -> bool {
            false
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tty7-gh-{name}-{}", std::process::id()));
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

    fn settle(
        app: &Entity<Tty7App>,
        vcx: &mut VisualTestContext,
        what: &str,
        until: impl Fn(&Tty7App) -> bool,
    ) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            app.update_in(vcx, |_, _, cx| cx.notify());
            vcx.background_executor.run_until_parked();
            if app.update_in(vcx, |app, _, _| until(app)) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "never settled: {what}"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[gpui::test]
    fn the_tab_lists_and_opens_an_issue_without_touching_the_network(cx: &mut TestAppContext) {
        let root = scratch("list");
        git(&root, &["init", "--quiet"]);
        git(
            &root,
            &[
                "remote",
                "add",
                "origin",
                "git@github.com:someone/widgets.git",
            ],
        );
        git(
            &root,
            &[
                "remote",
                "add",
                "upstream",
                "https://github.com/acme/widgets",
            ],
        );

        let fake = Arc::new(Fake {
            asked: Mutex::new(Vec::new()),
        });
        let (app, mut vcx, mut pane) = test_window::harness_with_pane(cx);
        let transport: Arc<dyn Transport> = fake.clone();
        app.update_in(&mut vcx, |app, _, cx| {
            app.github.connector = Some(Arc::new(move || Connection {
                transport: transport.clone(),
            }));
            app.right_panel_visible = true;
            app.right_panel_tab = RightPanelTab::GitHub;
            cx.notify();
        });
        DaemonMsg::Cwd(root.clone())
            .encode(&mut pane)
            .expect("the pane's socket takes the cwd");

        // A fork setup: upstream wins over origin.
        settle(&app, &mut vcx, "the issue list", |app| {
            app.github
                .lists
                .iter()
                .any(|(q, l)| q.slug.full() == "acme/widgets" && q.kind == Kind::Issues && l.loaded)
        });
        let numbers: Vec<u64> = app.update_in(&mut vcx, |app, _, _| {
            let (_, list) = app.github.lists.iter().next().unwrap();
            list.items.iter().map(|i| i.number).collect()
        });
        assert_eq!(numbers, vec![7], "the pull request is not an issue");

        let slug = app.update_in(&mut vcx, |app, _, cx| {
            let slug = app.github.lists.keys().next().unwrap().slug.clone();
            app.github_open_detail(slug.clone(), 7, cx);
            slug
        });
        settle(&app, &mut vcx, "the detail", |app| {
            app.github
                .details
                .get(&(slug.clone(), 7))
                .is_some_and(|d| d.detail.is_some())
        });
        let comments = app.update_in(&mut vcx, |app, _, _| {
            app.github.details[&(slug.clone(), 7)]
                .detail
                .as_ref()
                .unwrap()
                .comments
                .len()
        });
        assert_eq!(comments, 1);

        let asked = fake.asked.lock().unwrap().clone();
        assert!(
            asked.iter().all(|p| p.starts_with("/repos/acme/widgets/")),
            "{asked:?}"
        );
        // Settled: nothing is asked for again frame after frame.
        let before = asked.len();
        for _ in 0..5 {
            app.update_in(&mut vcx, |_, _, cx| cx.notify());
            vcx.background_executor.run_until_parked();
        }
        assert_eq!(fake.asked.lock().unwrap().len(), before);

        test_window::quiesce(&mut vcx, Some(&root));
        let _ = std::fs::remove_dir_all(&root);
    }
}

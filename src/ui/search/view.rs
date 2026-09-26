//! The modal: a row of tabs over one searchable list.

use std::rc::Rc;

use gpui::{
    App, ClickEvent, Context, Entity, EventEmitter, MouseButton, MouseDownEvent, ScrollStrategy,
    Subscription, Task, Window, div, prelude::*, px,
};
use gpui_component::{
    ActiveTheme as _, IndexPath, h_flex,
    list::{List, ListDelegate, ListEvent, ListItem, ListState},
    v_flex,
};

use super::SearchTab;
use super::command::{CommandKind, Item};
use super::sources::{Catalog, Row, Section, plain};
use crate::core::actions::{SearchNextTab, SearchPrevTab};
use crate::ui::i18n::{L10nKey, t, t_fmt};

/// What the list is showing: one of the tabs, or the theme picker one of the
/// Actions rows opens. The picker has no tabs — Escape goes back to the one it
/// came from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Scope {
    Tab(SearchTab),
    Themes,
}

pub(crate) struct SearchDelegate {
    catalog: Rc<Catalog>,
    scope: Scope,
    /// The theme picker's rows, built when it opens.
    themes: Vec<Item>,
    query: String,
    sections: Vec<Section>,
    selected: Option<IndexPath>,
}

impl SearchDelegate {
    fn new(catalog: Rc<Catalog>, scope: Scope, themes: Vec<Item>, cx: &App) -> Self {
        let mut this = Self {
            catalog,
            scope,
            themes,
            query: String::new(),
            sections: Vec::new(),
            selected: None,
        };
        this.refresh(cx);
        this
    }

    /// Re-runs the current query against the current scope.
    fn refresh(&mut self, cx: &App) {
        self.sections = match self.scope {
            Scope::Tab(tab) => self.catalog.sections(tab, &self.query, cx),
            Scope::Themes => plain(&self.themes, &self.query),
        };
    }

    fn row_at(&self, ix: IndexPath) -> Option<&Row> {
        self.sections.get(ix.section)?.rows.get(ix.row)
    }

    fn selected_item(&self) -> Option<&Item> {
        self.selected.and_then(|ix| self.row_at(ix)?.item())
    }

    fn first_row(&self) -> Option<IndexPath> {
        let section = self.sections.iter().position(|s| !s.rows.is_empty())?;
        Some(IndexPath::new(0).section(section))
    }

    fn render_row(&self, ix: IndexPath, item: &Item, cx: &App) -> gpui::AnyElement {
        let (kbd_bg, border, muted) = {
            let t = cx.theme();
            (t.secondary.opacity(0.6), t.border, t.muted_foreground)
        };

        let mut left = h_flex().flex_1().min_w_0().items_center().gap_2();
        if let Some(avatar) = item.avatar {
            left = left.child(crate::ui::tab_strip::avatar(
                ("search-avatar", ix.section * 1000 + ix.row),
                avatar,
                AVATAR,
                cx,
            ));
        }
        left = left.child(div().truncate().child(item.title.clone()));
        if let Some(subtitle) = item.subtitle.clone() {
            left = left.child(div().truncate().text_xs().text_color(muted).child(subtitle));
        }

        let mut right = h_flex().flex_shrink_0().items_center().gap_2();
        if let Some(note) = item.note.clone() {
            right = right.child(div().text_xs().text_color(muted).child(note));
        }
        if item.kind.edit_variant().is_some() {
            right = right.child(
                h_flex()
                    .items_center()
                    .gap_1()
                    .text_xs()
                    .text_color(muted)
                    .child(t(L10nKey::EditHint))
                    .child(crate::ui::keymap::key_tokens(EDIT_GESTURE).join("")),
            );
        }
        if let Some(spec) = item.kind.key_spec(cx) {
            let tokens = crate::ui::keymap::key_tokens(&spec);
            right = right.child(h_flex().gap_1().children(tokens.into_iter().map(move |t| {
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .min_w(px(20.))
                    .h(px(20.))
                    .px_1()
                    .rounded_md()
                    .bg(kbd_bg)
                    .border_1()
                    .border_color(border)
                    .text_xs()
                    .text_color(muted)
                    .child(t)
            })));
        }

        h_flex()
            .w_full()
            .gap_3()
            .items_center()
            .justify_between()
            .child(left)
            .child(right)
            .into_any_element()
    }
}

impl ListDelegate for SearchDelegate {
    type Item = ListItem;

    fn sections_count(&self, _cx: &App) -> usize {
        self.sections.len().max(1)
    }

    fn items_count(&self, section: usize, _cx: &App) -> usize {
        self.sections
            .get(section)
            .map(|s| s.rows.len())
            .unwrap_or(0)
    }

    fn perform_search(
        &mut self,
        query: &str,
        window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Task<()> {
        self.query = query.to_string();
        self.refresh(cx);
        // Through `set_selected_index`, not by hand: the row index may not have
        // moved, but the row under it has, and the theme picker previews the
        // row — not the index.
        self.selected = None;
        let first = self.first_row();
        self.set_selected_index(first, window, cx);
        Task::ready(())
    }

    fn render_section_header(
        &mut self,
        section: usize,
        _window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Option<impl IntoElement> {
        let title = self.sections.get(section)?.title.clone()?;
        Some(
            h_flex()
                .h(px(ROW_H))
                .px(px(LABEL_INSET))
                .items_center()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(title),
        )
    }

    fn render_empty(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> impl IntoElement {
        // The SSH hint only where typing user@host would actually connect. A
        // theme picker with no matches teaching SSH is a crossed wire (#602).
        let hint = match self.scope {
            Scope::Tab(SearchTab::All | SearchTab::Hosts) => t(L10nKey::ConnectSshHint),
            _ => t(L10nKey::PaletteTryDifferentSearch),
        };
        v_flex()
            .py_8()
            .gap_1()
            .items_center()
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .child(t(L10nKey::SearchNoResults))
            .child(div().text_xs().child(hint))
    }

    fn render_item(
        &mut self,
        ix: IndexPath,
        _window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Option<Self::Item> {
        let content = match self.row_at(ix)? {
            Row::Item(item) => self.render_row(ix, item, cx),
            Row::More { tab, hidden } => h_flex()
                .w_full()
                .items_center()
                .justify_between()
                .text_color(cx.theme().muted_foreground)
                .child(t_fmt(
                    L10nKey::SearchMoreIn,
                    &[("count", &hidden.to_string()), ("tab", tab.title())],
                ))
                .child(
                    div()
                        .text_xs()
                        .child(crate::ui::keymap::key_tokens("tab").join("")),
                )
                .into_any_element(),
        };
        Some(
            ListItem::new(("search-row", ix.section * 1000 + ix.row))
                .selected(Some(ix) == self.selected)
                .h(px(ROW_H))
                .mx(px(ROW_MX))
                .rounded(crate::ui::rounding::ROW_RADIUS)
                .text_size(gpui::rems(13. / 16.))
                .child(content),
        )
    }

    fn set_selected_index(
        &mut self,
        ix: Option<IndexPath>,
        window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) {
        // After every search the list re-picks its row from the rows it drew
        // last frame, and when that frame drew none — the search had only just
        // opened, or the previous query found nothing — it picks none. Return
        // then did nothing, on a list with rows in it. Put the first row back
        // once the list is done.
        if ix.is_none()
            && let Some(first) = self.first_row()
        {
            cx.defer_in(window, move |state, window, cx| {
                if state.selected_index().is_none() {
                    state.set_selected_index(Some(first), window, cx);
                }
            });
        }
        let moved = self.selected != ix;
        self.selected = ix;
        // The list only emits `Select` for the arrow keys; a query that re-arms
        // the first row moves the highlight silently. The theme picker previews
        // whatever is highlighted, so it has to hear about both.
        if moved && let Some(ix) = ix {
            cx.emit(ListEvent::Select(ix));
        }
        cx.notify();
    }
}

pub enum SearchEvent {
    Confirm(CommandKind),
    Dismiss,
    /// Show the theme at this preset index without persisting it: the theme
    /// picker previews the highlighted row while it stays open.
    PreviewTheme(usize),
    /// Put back the theme that was live before the preview started.
    CancelThemePreview,
}

pub struct SearchView {
    list: Entity<ListState<SearchDelegate>>,
    catalog: Rc<Catalog>,
    /// The tab showing — or, in the theme picker, the one Escape returns to.
    tab: SearchTab,
    /// What was typed when the theme picker opened, put back when it closes.
    parked_query: Option<String>,
    /// Preset index the theme picker is currently previewing, so the same
    /// theme is not re-applied on every redundant selection event.
    previewing: Option<usize>,
    _sub: Subscription,
}

impl SearchView {
    /// The search, on `tab`, with `query` already typed.
    ///
    /// The seed goes through the search field rather than around it, so what
    /// the reader sees is a search in the state they would have typed it into:
    /// the text is there, rows are ranked against it, and the next keystroke
    /// goes on refining instead of starting over.
    pub fn new(
        catalog: Catalog,
        tab: SearchTab,
        query: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let catalog = Rc::new(catalog);
        let delegate = SearchDelegate::new(catalog.clone(), Scope::Tab(tab), Vec::new(), cx);
        let list = Self::build_list(delegate, None, window, cx);
        if !query.is_empty() {
            list.update(cx, |state, cx| state.set_query(query, window, cx));
        }
        let _sub = cx.subscribe_in(&list, window, Self::on_list_event);
        Self {
            list,
            catalog,
            tab,
            parked_query: None,
            previewing: None,
            _sub,
        }
    }

    fn in_themes(&self) -> bool {
        self.parked_query.is_some()
    }

    fn build_list(
        delegate: SearchDelegate,
        selected: Option<IndexPath>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ListState<SearchDelegate>> {
        let first = selected.or_else(|| delegate.first_row());
        let list = cx.new(|cx| ListState::new(delegate, window, cx).searchable(true));
        list.update(cx, |state, cx| {
            // `ListState::new` starts with nothing selected, and it only picks a
            // row once a query changes. Opening the search and pressing Return
            // therefore did nothing at all, and until then no row showed what
            // Return was aimed at.
            state.set_selected_index(first, window, cx);
            if selected.is_some() {
                state.scroll_to_selected_item(window, cx);
            }
            state.focus(window, cx);
        });
        list
    }

    fn replace_list(
        &mut self,
        list: Entity<ListState<SearchDelegate>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self._sub = cx.subscribe_in(&list, window, Self::on_list_event);
        self.list = list;
        cx.notify();
    }

    /// Shows `tab`, keeping what is typed — or replacing it with `query`.
    ///
    /// In place, on the same list: the field keeps its text and focus, and
    /// only the rows under it change.
    pub(crate) fn set_tab(
        &mut self,
        tab: SearchTab,
        query: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.in_themes() {
            return;
        }
        self.tab = tab;
        self.list.update(cx, |state, cx| {
            state.delegate_mut().scope = Scope::Tab(tab);
            if let Some(query) = query {
                state.set_query(query, window, cx);
            }
            // `set_query` searches only when the text changed; the tab did.
            state.delegate_mut().refresh(cx);
            let first = state.delegate().first_row();
            state.set_selected_index(first, window, cx);
            state.scroll_to_item(IndexPath::default(), ScrollStrategy::Top, window, cx);
        });
        cx.notify();
    }

    fn step_tab(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.set_tab(self.tab.step(forward), None, window, cx);
    }

    fn open_themes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.parked_query = Some(self.list.read(cx).delegate().query.clone());
        // Open on the theme already in use: the picker previews the
        // highlighted row, and merely opening it must not change what the
        // window looks like.
        self.previewing = Item::active_theme_index(cx);
        let delegate =
            SearchDelegate::new(self.catalog.clone(), Scope::Themes, Item::themes(cx), cx);
        let list = Self::build_list(delegate, self.previewing.map(IndexPath::new), window, cx);
        self.replace_list(list, window, cx);
    }

    fn close_themes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Backing out of the picker is not a choice: whatever was previewed
        // goes back to what it was.
        self.previewing = None;
        cx.emit(SearchEvent::CancelThemePreview);
        let query = self.parked_query.take().unwrap_or_default();
        let delegate =
            SearchDelegate::new(self.catalog.clone(), Scope::Tab(self.tab), Vec::new(), cx);
        let list = Self::build_list(delegate, None, window, cx);
        if !query.is_empty() {
            list.update(cx, |state, cx| state.set_query(&query, window, cx));
        }
        self.replace_list(list, window, cx);
    }

    fn selected_edit_command(&self, cx: &App) -> Option<CommandKind> {
        self.list
            .read(cx)
            .delegate()
            .selected_item()
            .and_then(|item| item.kind.edit_variant())
    }

    fn on_list_event(
        &mut self,
        list: &Entity<ListState<SearchDelegate>>,
        ev: &ListEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match ev {
            ListEvent::Confirm(ix) => {
                let row = list.read(cx).delegate().row_at(*ix).cloned();
                match row {
                    Some(Row::More { tab, .. }) => self.set_tab(tab, None, window, cx),
                    Some(Row::Item(item)) => match item.kind {
                        CommandKind::OpenThemePicker => self.open_themes(window, cx),
                        // What was typed found this row; it is not an address.
                        CommandKind::SearchHosts => {
                            self.set_tab(SearchTab::Hosts, Some(""), window, cx)
                        }
                        kind => cx.emit(SearchEvent::Confirm(kind)),
                    },
                    None => cx.emit(SearchEvent::Dismiss),
                }
            }
            ListEvent::Cancel => match self.in_themes() {
                true => self.close_themes(window, cx),
                false => cx.emit(SearchEvent::Dismiss),
            },
            ListEvent::Select(ix) => {
                if self.in_themes()
                    && let Some(Row::Item(item)) = list.read(cx).delegate().row_at(*ix)
                    && let CommandKind::SetTheme(i) = item.kind
                    && self.previewing != Some(i)
                {
                    self.previewing = Some(i);
                    cx.emit(SearchEvent::PreviewTheme(i));
                }
            }
        }
    }

    fn render_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (active_bg, hover_bg, fg, muted) = (
            theme.secondary,
            theme.secondary.opacity(0.5),
            theme.foreground,
            theme.muted_foreground,
        );
        h_flex()
            .px(px(ROW_MX + 4.))
            .pt(px(8.))
            .gap_1()
            .items_center()
            .children(SearchTab::ORDER.into_iter().enumerate().map(|(i, tab)| {
                let active = tab == self.tab;
                div()
                    .id(("search-tab", i))
                    .h(px(24.))
                    .px(px(10.))
                    .flex()
                    .items_center()
                    .rounded_md()
                    .text_size(gpui::rems(12. / 16.))
                    .cursor_pointer()
                    .map(|d| match active {
                        true => d
                            .bg(active_bg)
                            .text_color(fg)
                            .font_weight(gpui::FontWeight::MEDIUM),
                        false => d.text_color(muted).hover(move |d| d.bg(hover_bg)),
                    })
                    .child(tab.title())
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.set_tab(tab, None, window, cx);
                        this.list.update(cx, |state, cx| state.focus(window, cx));
                    }))
            }))
            .child(div().flex_1())
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(crate::ui::keymap::key_tokens("tab").join("")),
            )
    }
}

impl EventEmitter<SearchEvent> for SearchView {}

const ROW_H: f32 = 34.;

/// Left inset of a row's *label*, so a section header can start on the same
/// pixel column as the rows it introduces. A row is a `ListItem` inset by
/// `ROW_MX` whose own padding is `px_3`; a header has neither, so it has to
/// carry the sum itself.
const ROW_MX: f32 = 5.;
const LABEL_INSET: f32 = ROW_MX + 12.;

const AVATAR: f32 = 20.;

/// The tab row's height with its padding, reserved out of the list's.
const TABS_H: f32 = 32.;

/// The chord that opens the selected row for editing instead of running it.
///
/// It cannot be `→`: gpui-component's `Input` binds bare `right` to MoveRight
/// in its own key context, so with the query field focused the search never
/// sees the key — the old `→ edit` badge was advertising a gesture that could
/// not fire. `⌘↵` is no better; the app binds it to ToggleFullscreen, which
/// wins for the same reason. `secondary-e` is claimed by neither.
const EDIT_GESTURE: &str = "secondary-e";

/// Matches `EDIT_GESTURE` against a live keystroke. Keep the two in step.
fn is_edit_gesture(ks: &gpui::Keystroke) -> bool {
    if ks.key != "e" {
        return false;
    }
    let m = &ks.modifiers;
    let secondary = if cfg!(target_os = "macos") {
        m.platform
    } else {
        m.control
    };
    secondary && !m.shift && !m.alt
}

const VISIBLE_ROWS: f32 = 12.;

/// The key context the search's own bindings (Tab, Shift-Tab) live in.
pub(crate) const KEY_CONTEXT: &str = "Search";

impl Render for SearchView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let scrim = crate::ui::presets::scrim_fill(cx);
        let tabs = !self.in_themes();

        let viewport = window.viewport_size();
        let top = (viewport.height.as_f32() * 0.16).clamp(16., 120.);
        // Reserve space for the tab row, the search field and the card's
        // padding, including when a split-screen window is shorter than the
        // full list.
        let chrome = 88. + if tabs { TABS_H } else { 0. };
        let list_max_h =
            px((viewport.height.as_f32() - top - chrome).clamp(ROW_H, ROW_H * VISIBLE_ROWS + 4.));
        let placeholder = match tabs {
            true => self.tab.placeholder(),
            false => t(L10nKey::SearchTheme),
        };
        let card = v_flex()
            .w(px((viewport.width.as_f32() - 32.).clamp(0., 640.)))
            .map(|panel| crate::ui::theme::floating_surface(panel, cx))
            .overflow_hidden()
            .pb_1()
            .when(tabs, |card| card.child(self.render_tabs(cx)))
            .child(
                List::new(&self.list)
                    .search_placeholder(placeholder)
                    .py_1()
                    .max_h(list_max_h),
            );

        div()
            .absolute()
            .inset_0()
            .flex()
            .items_start()
            .justify_center()
            .pt(px(top))
            .bg(scrim)
            .key_context(KEY_CONTEXT)
            .on_action(
                cx.listener(|this, _: &SearchNextTab, window, cx| this.step_tab(true, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SearchPrevTab, window, cx| this.step_tab(false, window, cx)),
            )
            .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, _window, cx| {
                if is_edit_gesture(&ev.keystroke)
                    && let Some(edit) = this.selected_edit_command(cx)
                {
                    cx.stop_propagation();
                    cx.emit(SearchEvent::Confirm(edit));
                }
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_this, _: &MouseDownEvent, _window, cx| {
                    cx.emit(SearchEvent::Dismiss);
                }),
            )
            .child(div().occlude().child(card))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::app::{Tty7App, test_window::harness_with_tabs};
    use gpui::{TestAppContext, VisualTestContext};

    fn open(app: &Entity<Tty7App>, vcx: &mut VisualTestContext) -> Entity<SearchView> {
        app.read_with(vcx, |app, _| {
            app.search.clone().expect("the search is open")
        })
    }

    fn first_kind(view: &Entity<SearchView>, vcx: &mut VisualTestContext) -> Option<CommandKind> {
        view.read_with(vcx, |view, cx| {
            let delegate = view.list.read(cx).delegate();
            Some(delegate.row_at(delegate.first_row()?)?.item()?.kind.clone())
        })
    }

    #[gpui::test]
    fn tab_walks_the_tabs_and_keeps_what_was_typed(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 2);
        crate::ui::i18n::set_locale("en");
        app.update_in(&mut vcx, |app, window, cx| {
            app.open_search(SearchTab::All, "split right", window, cx)
        });
        vcx.run_until_parked();
        let view = open(&app, &mut vcx);

        vcx.simulate_keystrokes("tab");
        vcx.run_until_parked();
        view.read_with(&vcx, |view, cx| {
            assert_eq!(view.tab, SearchTab::Actions);
            assert_eq!(view.list.read(cx).delegate().query, "split right");
        });
        assert_eq!(first_kind(&view, &mut vcx), Some(CommandKind::SplitRight));

        // Backwards, and round the end.
        vcx.simulate_keystrokes("shift-tab shift-tab");
        vcx.run_until_parked();
        view.read_with(&vcx, |view, _| assert_eq!(view.tab, SearchTab::Hosts));
        assert!(
            app.read_with(&vcx, |app, _| app.search.is_some()),
            "Tab stays inside the search instead of walking focus out of it"
        );
    }

    /// Opened with nothing typed, Return goes back to the tab you were last
    /// in — the All tab leads with this window's other tabs.
    #[gpui::test]
    fn return_on_an_empty_search_goes_to_the_previous_tab(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 3);
        app.update_in(&mut vcx, |app, _, _| {
            app.tabs[1].last_used.set(5);
            app.tabs[2].last_used.set(9);
        });
        app.update_in(&mut vcx, |app, window, cx| {
            app.open_search(SearchTab::All, "", window, cx)
        });
        vcx.run_until_parked();
        let view = open(&app, &mut vcx);
        assert!(
            matches!(
                first_kind(&view, &mut vcx),
                Some(CommandKind::GoToTab { .. })
            ),
            "the first row is a tab"
        );

        vcx.simulate_keystrokes("enter");
        vcx.run_until_parked();
        app.read_with(&vcx, |app, _| {
            assert!(app.search.is_none(), "picking a row closes the search");
            assert_eq!(app.active, 2, "back to the tab used before this one");
        });
    }

    /// "SSH: Add Connection…" was a second input box. It is the Hosts tab
    /// now — and whatever found the row is not an address, so it goes.
    #[gpui::test]
    fn add_connection_moves_to_an_empty_hosts_tab(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 1);
        crate::ui::i18n::set_locale("en");
        app.update_in(&mut vcx, |app, window, cx| {
            app.open_search(SearchTab::Actions, "ssh-add-connection", window, cx)
        });
        vcx.run_until_parked();
        let view = open(&app, &mut vcx);
        assert_eq!(first_kind(&view, &mut vcx), Some(CommandKind::SearchHosts));

        vcx.simulate_keystrokes("enter");
        vcx.run_until_parked();
        view.read_with(&vcx, |view, cx| {
            assert_eq!(view.tab, SearchTab::Hosts);
            assert_eq!(view.list.read(cx).delegate().query, "");
        });
    }

    /// The list re-picks its row from what it drew last frame, and a query
    /// that found nothing drew nothing — so the query typed next came up with
    /// rows and nothing selected, and Return did nothing.
    #[gpui::test]
    fn a_query_after_one_that_found_nothing_still_arms_its_first_row(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 1);
        crate::ui::i18n::set_locale("en");
        app.update_in(&mut vcx, |app, window, cx| {
            app.open_search(SearchTab::Actions, "", window, cx)
        });
        vcx.run_until_parked();
        let view = open(&app, &mut vcx);

        vcx.simulate_input("split rightq");
        vcx.run_until_parked();
        assert_eq!(first_kind(&view, &mut vcx), None, "nothing matches");
        // One keystroke back to a query with rows: the one search that runs
        // right after a frame that drew nothing.
        vcx.simulate_keystrokes("backspace");
        vcx.run_until_parked();

        let selected = view.read_with(&vcx, |view, cx| view.list.read(cx).selected_index());
        assert!(selected.is_some(), "Return has a row to run");
        vcx.simulate_keystrokes("enter");
        vcx.run_until_parked();
        assert!(app.read_with(&vcx, |app, _| app.search.is_none()));
    }
}

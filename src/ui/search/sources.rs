//! What each tab holds, and how the All tab puts them side by side.
//!
//! A tab is a [`Source`]: it knows what to show before anything is typed and
//! how to rank its rows against a query. Adding a tab is adding a source — the
//! view, the All tab and the keyboard walk come with it.

use gpui::{App, SharedString};

use super::SearchTab;
use super::command::{CommandGroup, CommandKind, Item};
use super::score::{frecency_bonus, item_score};
use crate::core::config::Config;
use crate::core::ssh_profile::parse_quick_connect;
use crate::ui::i18n::{L10nKey, t, t_fmt};

/// How many recently used actions lead the Actions tab.
const RECENT_ROWS: usize = 5;

/// How many rows one tab may put on the All tab before the rest fold into a
/// "more" row that opens the tab itself.
pub(crate) const ALL_TAB_ROWS: usize = 5;

/// Rows under one header. A section never holds zero rows.
#[derive(Clone)]
pub(crate) struct Section {
    pub title: Option<SharedString>,
    pub rows: Vec<Row>,
}

#[derive(Clone)]
pub(crate) enum Row {
    Item(Item),
    /// The All tab's way into a tab it only showed the top of.
    More {
        tab: SearchTab,
        hidden: usize,
    },
}

impl Row {
    pub(crate) fn item(&self) -> Option<&Item> {
        match self {
            Row::Item(item) => Some(item),
            Row::More { .. } => None,
        }
    }
}

/// One tab's rows.
pub(crate) trait Source {
    fn tab(&self) -> SearchTab;

    /// The tab with nothing typed.
    fn browse(&self, cx: &App) -> Vec<Section>;

    /// The few rows that stand for this tab on the All tab before anything is
    /// typed. Empty is fine: the tab then shows only once a query finds it.
    fn highlights(&self, cx: &App) -> Vec<Item>;

    /// Rows answering `query`, best first, each with the score that put it
    /// there. The All tab compares these scores across tabs, so every source
    /// scores with [`item_score`] and only nudges it.
    fn search(&self, query: &str, cx: &App) -> Vec<(i32, Item)>;
}

/// Everything the search offers, gathered once when it opens. The window
/// builds it (`Tty7App::search_catalog`) because most of it — the tabs, the
/// shells, what the chrome is showing — is the window's to know.
#[derive(Clone, Default)]
pub(crate) struct Catalog {
    /// In group order, which is what the Actions tab's sections follow.
    pub actions: Vec<Item>,
    /// Open tabs first, each workspace's under its name, then the ways to
    /// open a new one.
    pub terminals: Vec<Item>,
    pub hosts: Vec<Item>,
}

impl Catalog {
    pub(crate) fn new(mut actions: Vec<Item>, terminals: Vec<Item>, hosts: Vec<Item>) -> Self {
        // Stable, so a group keeps the order its rows were listed in. The
        // window appends the rows only it can judge after the fixed ones; this
        // is what files them under their group instead of at the end.
        actions.sort_by_key(|item| {
            item.group
                .and_then(|g| CommandGroup::ORDER.iter().position(|o| *o == g))
                .unwrap_or(usize::MAX)
        });
        Self {
            actions,
            terminals,
            hosts,
        }
    }

    pub(crate) fn source(&self, tab: SearchTab) -> Option<Box<dyn Source + '_>> {
        match tab {
            SearchTab::All => None,
            SearchTab::Actions => Some(Box::new(Actions(&self.actions))),
            SearchTab::Terminals => Some(Box::new(Terminals(&self.terminals))),
            SearchTab::Hosts => Some(Box::new(Hosts(&self.hosts))),
        }
    }

    /// The sections `tab` shows for `query`.
    pub(crate) fn sections(&self, tab: SearchTab, query: &str, cx: &App) -> Vec<Section> {
        let query = query.trim();
        if let Some(source) = self.source(tab) {
            return match query.is_empty() {
                true => source.browse(cx),
                false => untitled(source.search(query, cx)),
            };
        }
        self.all(query, cx)
    }

    /// Every tab's top rows, each under the tab's name.
    ///
    /// Tabs are ordered by their best row, not fixed: the Return key takes the
    /// first row, so the first row has to be the best answer anywhere. They
    /// are comparable because every source scores with the one scorer.
    fn all(&self, query: &str, cx: &App) -> Vec<Section> {
        let tabs = SearchTab::ORDER
            .into_iter()
            .filter_map(|tab| self.source(tab));
        if query.is_empty() {
            // Terminals first: before anything is typed the likeliest thing
            // wanted is the tab you were just in.
            let mut sources: Vec<_> = tabs.collect();
            sources.sort_by_key(|s| s.tab() != SearchTab::Terminals);
            return sources
                .into_iter()
                .filter_map(|s| {
                    let rows: Vec<Row> = s.highlights(cx).into_iter().map(Row::Item).collect();
                    (!rows.is_empty()).then(|| Section {
                        title: Some(s.tab().title().into()),
                        rows,
                    })
                })
                .collect();
        }
        let mut found: Vec<(i32, Section)> = tabs
            .filter_map(|s| {
                let hits = s.search(query, cx);
                let best = hits.first()?.0;
                let hidden = hits.len().saturating_sub(ALL_TAB_ROWS);
                let mut rows: Vec<Row> = hits
                    .into_iter()
                    .take(ALL_TAB_ROWS)
                    .map(|(_, item)| Row::Item(item))
                    .collect();
                if hidden > 0 {
                    rows.push(Row::More {
                        tab: s.tab(),
                        hidden,
                    });
                }
                Some((
                    best,
                    Section {
                        title: Some(s.tab().title().into()),
                        rows,
                    },
                ))
            })
            .collect();
        found.sort_by_key(|(best, _)| std::cmp::Reverse(*best));
        found.into_iter().map(|(_, section)| section).collect()
    }
}

/// A search result: one section, no header — the order is the answer.
fn untitled(hits: Vec<(i32, Item)>) -> Vec<Section> {
    if hits.is_empty() {
        return Vec::new();
    }
    vec![Section {
        title: None,
        rows: hits.into_iter().map(|(_, item)| Row::Item(item)).collect(),
    }]
}

/// Consecutive rows that share a section label, under that label.
fn by_section<'a>(items: impl IntoIterator<Item = &'a Item>) -> Vec<Section> {
    let mut out: Vec<Section> = Vec::new();
    for item in items {
        match out.last_mut() {
            Some(last) if last.title == item.section => last.rows.push(Row::Item(item.clone())),
            _ => out.push(Section {
                title: item.section.clone(),
                rows: vec![Row::Item(item.clone())],
            }),
        }
    }
    out
}

/// Every row that matches, best first. Stable, so rows that score alike keep
/// the order the tab lists them in — most recently used, for most tabs.
fn rank(items: &[Item], query: &str, bonus: impl Fn(&Item) -> i32) -> Vec<(i32, Item)> {
    let mut hits: Vec<(i32, Item)> = items
        .iter()
        .filter_map(|item| Some((item_score(query, item)? + bonus(item), item.clone())))
        .collect();
    hits.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    hits
}

fn action_usage(item: &Item, cx: &App) -> f64 {
    let cfg = cx.global::<Config>();
    item.kind
        .id()
        .and_then(|id| cfg.command_frecency.get(id))
        .map(|u| u.score(crate::core::config::unix_now()))
        .unwrap_or(0.0)
}

struct Actions<'a>(&'a [Item]);

impl Actions<'_> {
    fn recent(&self, cx: &App) -> Vec<&Item> {
        let mut used: Vec<(f64, &Item)> = self
            .0
            .iter()
            .map(|item| (action_usage(item, cx), item))
            .filter(|(score, _)| *score > 0.0)
            .collect();
        used.sort_by(|a, b| b.0.total_cmp(&a.0));
        used.truncate(RECENT_ROWS);
        used.into_iter().map(|(_, item)| item).collect()
    }
}

impl Source for Actions<'_> {
    fn tab(&self) -> SearchTab {
        SearchTab::Actions
    }

    fn browse(&self, cx: &App) -> Vec<Section> {
        let recent = self.recent(cx);
        let mut out = Vec::new();
        if !recent.is_empty() {
            out.push(Section {
                title: Some(t(L10nKey::CmdRecent).into()),
                rows: recent
                    .iter()
                    .map(|item| Row::Item((*item).clone()))
                    .collect(),
            });
        }
        // Promoting an action to Recent moves it; it does not clone it.
        // Leaving it in its group too meant the five rows you use most were
        // the five rows the list showed twice.
        out.extend(by_section(
            self.0
                .iter()
                .filter(|item| !recent.iter().any(|r| r.kind == item.kind)),
        ));
        out
    }

    fn highlights(&self, cx: &App) -> Vec<Item> {
        self.recent(cx).into_iter().cloned().collect()
    }

    fn search(&self, query: &str, cx: &App) -> Vec<(i32, Item)> {
        // Frecency ordered the zero-query list and was then thrown away the
        // moment a character was typed, so the action someone runs every day
        // stopped floating exactly when they started reaching for it.
        rank(self.0, query, |item| frecency_bonus(action_usage(item, cx)))
    }
}

struct Terminals<'a>(&'a [Item]);

impl Source for Terminals<'_> {
    fn tab(&self) -> SearchTab {
        SearchTab::Terminals
    }

    fn browse(&self, _cx: &App) -> Vec<Section> {
        by_section(self.0)
    }

    /// This window's other tabs, most recently used first — what Ctrl-Tab
    /// would offer.
    fn highlights(&self, _cx: &App) -> Vec<Item> {
        let first = self.0.first().and_then(|item| item.section.clone());
        self.0
            .iter()
            .take_while(|item| item.section == first)
            .filter(|item| matches!(item.kind, CommandKind::GoToTab { .. }))
            .take(ALL_TAB_ROWS)
            .cloned()
            .collect()
    }

    fn search(&self, query: &str, _cx: &App) -> Vec<(i32, Item)> {
        rank(self.0, query, |_| 0)
    }
}

/// A typed address outranks any saved host: typing one is saying where to go.
const TYPED_ADDRESS_SCORE: i32 = 10_000;

struct Hosts<'a>(&'a [Item]);

impl Source for Hosts<'_> {
    fn tab(&self) -> SearchTab {
        SearchTab::Hosts
    }

    fn browse(&self, _cx: &App) -> Vec<Section> {
        by_section(self.0)
    }

    fn highlights(&self, _cx: &App) -> Vec<Item> {
        self.0.iter().take(ALL_TAB_ROWS).cloned().collect()
    }

    fn search(&self, query: &str, _cx: &App) -> Vec<(i32, Item)> {
        let mut out: Vec<(i32, Item)> = typed_address_rows(query)
            .into_iter()
            .map(|item| (TYPED_ADDRESS_SCORE, item))
            .collect();
        out.extend(rank(self.0, query, |_| 0));
        out
    }
}

/// What a typed address offers: connect to it, or save it as a host. A bare
/// word gets nothing — it is far likelier a search than a host name — and so
/// does anything that does not parse, rather than a row that would fail.
///
/// A full `ssh` command line (`-p 2222 -J jump`) is not an address, but it is
/// just as clearly a place to go, so it gets its own row.
pub(crate) fn typed_address_rows(query: &str) -> Vec<Item> {
    let query = query.trim();
    // Whitespace first: an address has none, and the address parser would read
    // `ssh -p 2222 deploy@box` as the user `ssh -p 2222 deploy` on `box`.
    if query.contains(char::is_whitespace) {
        return match crate::ui::app::parse_ssh_connect_input(query) {
            Ok(_) => vec![Item::new(
                t_fmt(L10nKey::CmdSshConnectWithInput, &[("input", query)]),
                CommandKind::OpenSshConnect(query.to_string()),
            )],
            Err(_) => Vec::new(),
        };
    }
    if query.contains(['@', ':', '.']) && parse_quick_connect(query).is_some() {
        let target = query.to_string();
        return vec![
            Item::new(
                t_fmt(L10nKey::CmdQuickConnect, &[("target", &target)]),
                CommandKind::QuickConnect(target.clone()),
            ),
            Item::new(
                t_fmt(L10nKey::CmdQuickConnectSaveProfile, &[("target", &target)]),
                CommandKind::SaveQuickConnect(target),
            ),
        ];
    }
    Vec::new()
}

/// The Hosts tab: every saved host, most used first.
pub(crate) fn host_items(cx: &App) -> Vec<Item> {
    crate::ui::ssh_connect::ssh_profiles_by_frecency(cx)
        .into_iter()
        .map(|p| {
            let endpoint = crate::core::ssh_profile::to_connect_string(&p);
            let title = match p.name.is_empty() {
                true => endpoint.clone(),
                false => p.name.clone(),
            };
            let mut item = Item::new(title, CommandKind::ConnectSavedProfile(p.id));
            if item.title != endpoint {
                item = item.with_subtitle(endpoint);
            }
            item
        })
        .collect()
}

/// A flat list with no tabs of its own — the theme picker. Everything while
/// nothing is typed, in the order given; ranked once something is.
pub(crate) fn plain(items: &[Item], query: &str) -> Vec<Section> {
    let query = query.trim();
    match query.is_empty() {
        true => by_section(items),
        false => untitled(rank(items, query, |_| 0)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use tty7_core::core::machine::TabId;
    use tty7_core::core::session::WorkspaceId;

    fn titles(query: &str) -> Vec<String> {
        typed_address_rows(query)
            .into_iter()
            .map(|c| c.title)
            .collect()
    }

    #[test]
    fn a_bare_word_is_a_search_not_an_address() {
        assert!(titles("java").is_empty());
        assert!(titles("split").is_empty());
        assert!(titles("").is_empty());
    }

    #[test]
    fn an_address_offers_to_connect_and_to_save() {
        crate::ui::i18n::set_locale("en");
        for q in [
            "deploy@10.0.0.5",
            "host.example.com",
            "java:2222",
            "ssh://java",
            "[::1]:2222",
        ] {
            assert_eq!(
                titles(q),
                vec![
                    t_fmt(L10nKey::CmdQuickConnect, &[("target", q)]),
                    t_fmt(L10nKey::CmdQuickConnectSaveProfile, &[("target", q)]),
                ],
                "query {q:?}"
            );
        }
    }

    #[test]
    fn an_address_that_does_not_parse_offers_nothing() {
        assert!(titles("java:99999").is_empty());
        assert!(titles("@").is_empty());
    }

    /// What the old "SSH: Add Connection…" input took, the Hosts tab now
    /// takes as typed: a whole `ssh` command line.
    #[test]
    fn an_ssh_command_line_offers_to_connect_with_it() {
        let rows = typed_address_rows("ssh -p 2222 deploy@box");
        let [row] = rows.as_slice() else {
            panic!("one row, got {}", rows.len());
        };
        assert_eq!(
            row.kind,
            CommandKind::OpenSshConnect("ssh -p 2222 deploy@box".into())
        );
    }

    fn with_config(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        cx.update(|cx| {
            cx.set_global(Config::default());
            crate::ui::i18n::set_locale("en");
        });
    }

    fn terminal(title: &str, section: &str) -> Item {
        Item::new(
            title,
            CommandKind::GoToTab {
                workspace: WorkspaceId::new(),
                tab: TabId::new(),
            },
        )
        .in_section(section.to_string())
    }

    fn row_titles(section: &Section) -> Vec<String> {
        section
            .rows
            .iter()
            .map(|r| match r {
                Row::Item(item) => item.title.clone(),
                Row::More { tab, hidden } => format!("+{hidden} {tab:?}"),
            })
            .collect()
    }

    #[gpui::test]
    fn actions_are_filed_under_their_group_whatever_order_they_came_in(cx: &mut TestAppContext) {
        with_config(cx);
        let late = Item::new("Late", CommandKind::NewGroup).in_group(CommandGroup::TabsPanes);
        let first = Item::new("First", CommandKind::NewTab).in_group(CommandGroup::TabsPanes);
        let app = Item::new("Quit", CommandKind::Quit).in_group(CommandGroup::Application);
        let catalog = Catalog::new(vec![app, first, late], Vec::new(), Vec::new());
        cx.update(|cx| {
            let sections = catalog.sections(SearchTab::Actions, "", cx);
            let titles: Vec<_> = sections.iter().map(row_titles).collect();
            assert_eq!(
                titles,
                vec![vec!["First", "Late"], vec!["Quit"]],
                "Tabs & Panes leads, and a row appended late joins its group"
            );
        });
    }

    #[gpui::test]
    fn the_all_tab_leads_with_the_tab_holding_the_best_match(cx: &mut TestAppContext) {
        with_config(cx);
        let catalog = Catalog::new(
            vec![Item::new("Split Right", CommandKind::SplitRight)],
            vec![terminal("staging logs", "here")],
            vec![Item::new(
                "staging",
                CommandKind::ConnectSavedProfile(uuid::Uuid::new_v4()),
            )],
        );
        cx.update(|cx| {
            let sections = catalog.sections(SearchTab::All, "staging", cx);
            let headers: Vec<_> = sections.iter().filter_map(|s| s.title.clone()).collect();
            // The exact host name beats a prefix of the tab's label, and the
            // action does not match at all, so it is not there.
            assert_eq!(headers, vec!["Hosts", "Terminals"]);

            let sections = catalog.sections(SearchTab::All, "split", cx);
            assert_eq!(sections.len(), 1);
            assert_eq!(sections[0].title.as_deref(), Some("Actions"));
        });
    }

    #[gpui::test]
    fn the_all_tab_folds_a_long_answer_into_a_way_into_its_tab(cx: &mut TestAppContext) {
        with_config(cx);
        let terminals: Vec<Item> = (0..ALL_TAB_ROWS + 3)
            .map(|i| terminal(&format!("server {i}"), "here"))
            .collect();
        let catalog = Catalog::new(Vec::new(), terminals, Vec::new());
        cx.update(|cx| {
            let sections = catalog.sections(SearchTab::All, "server", cx);
            let rows = &sections[0].rows;
            assert_eq!(rows.len(), ALL_TAB_ROWS + 1);
            assert!(matches!(
                rows.last(),
                Some(Row::More {
                    tab: SearchTab::Terminals,
                    hidden: 3
                })
            ));
            // The tab itself holds every one.
            let own = catalog.sections(SearchTab::Terminals, "server", cx);
            assert_eq!(own[0].rows.len(), ALL_TAB_ROWS + 3);
        });
    }

    /// Before anything is typed the All tab offers this window's other tabs —
    /// the likeliest thing wanted — and never another workspace's, a launcher,
    /// or a header over nothing.
    #[gpui::test]
    fn the_empty_all_tab_leads_with_this_windows_other_tabs(cx: &mut TestAppContext) {
        with_config(cx);
        let mut terminals = vec![terminal("previous", "here"), terminal("older", "here")];
        terminals.push(terminal("elsewhere", "other workspace"));
        terminals
            .push(Item::new("Shell: zsh", CommandKind::OpenShell("zsh".into())).in_section("New"));
        let catalog = Catalog::new(Vec::new(), terminals, Vec::new());
        cx.update(|cx| {
            let sections = catalog.sections(SearchTab::All, "", cx);
            assert_eq!(
                sections.len(),
                1,
                "no recent actions and no hosts: no headers"
            );
            assert_eq!(sections[0].title.as_deref(), Some("Terminals"));
            assert_eq!(row_titles(&sections[0]), vec!["previous", "older"]);
        });
    }

    #[gpui::test]
    fn the_hosts_tab_puts_a_typed_address_before_any_saved_host(cx: &mut TestAppContext) {
        with_config(cx);
        let saved = Item::new(
            "prod",
            CommandKind::ConnectSavedProfile(uuid::Uuid::new_v4()),
        )
        .with_subtitle("deploy@prod.example.com");
        let catalog = Catalog::new(Vec::new(), Vec::new(), vec![saved]);
        cx.update(|cx| {
            let sections = catalog.sections(SearchTab::Hosts, "deploy@prod.example.com", cx);
            let kinds: Vec<_> = sections[0]
                .rows
                .iter()
                .filter_map(|r| r.item().map(|i| i.kind.clone()))
                .collect();
            assert!(matches!(kinds[0], CommandKind::QuickConnect(_)));
            assert!(matches!(kinds[1], CommandKind::SaveQuickConnect(_)));
            assert!(matches!(kinds[2], CommandKind::ConnectSavedProfile(_)));
        });
    }
}

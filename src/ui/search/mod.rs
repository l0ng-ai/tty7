//! Search Everywhere: one modal over everything the app can find — terminals,
//! sessions, hosts, actions — each in a tab of its own, and all of them at
//! once in the All tab. Files are Go to File's alone (see [`SearchTab::ORDER`]).
//!
//! - [`command`]: what a row runs ([`CommandKind`]) and the rows themselves.
//! - [`sources`]: what each tab holds and how it ranks against a query.
//! - [`files`]: the Files tab — the project's file index and quick open.
//! - [`score`]: the one fuzzy scorer every tab shares.
//! - [`view`]: the modal — the tab row, the list, the theme picker.

mod command;
pub(crate) mod files;
mod score;
mod sources;
mod view;

pub(crate) use command::{Avatar, ChromeState, CommandGroup, CommandKind, Item};
pub(crate) use files::{FileIndexStore, FileList};
pub(crate) use score::fuzzy_score;
pub(crate) use sources::{Catalog, LiveQuery, host_items};
pub(crate) use view::{KEY_CONTEXT, SearchEvent, SearchView};

use crate::ui::i18n::{L10nKey, t};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SearchTab {
    All,
    Files,
    Actions,
    Terminals,
    Sessions,
    Hosts,
    /// Go to Symbol: the symbols of the file in front of the editor. Like
    /// Files, reached by its chord (or the breadcrumbs), never by the row.
    Symbols,
    /// Places a language server found — the references to a symbol, or its
    /// several definitions (`ui::lsp`). Reached only by those commands.
    Locations,
}

impl SearchTab {
    /// The tab row, left to right, and the order Tab walks it.
    ///
    /// Files is not in it. Go to File (its own chord) still opens the search
    /// on it, but the row, Tab and the All tab leave it out: the right
    /// panel's Files tab is where the project is searched, names and text
    /// together, and a sample of paths among tabs and hosts answered nothing
    /// anyone had typed. Commands last, the way the rest of the row goes
    /// from the things you have to the things you can do.
    pub(crate) const ORDER: [SearchTab; 5] = [
        SearchTab::All,
        SearchTab::Terminals,
        SearchTab::Sessions,
        SearchTab::Hosts,
        SearchTab::Actions,
    ];

    /// The editor's row: finding your way around the code, as opposed to
    /// around the window. Go to File and Go to Symbol open on it, and Tab
    /// walks it the way it walks the window's row. Symbols shows only with a
    /// file in front; once something is typed it also lists what the file's
    /// language server finds across the project.
    pub(crate) const EDITOR_ORDER: [SearchTab; 2] = [SearchTab::Files, SearchTab::Symbols];

    pub(crate) fn in_editor_row(self) -> bool {
        Self::EDITOR_ORDER.contains(&self)
    }

    pub(crate) fn title(self) -> &'static str {
        t(match self {
            SearchTab::All => L10nKey::SearchTabAll,
            SearchTab::Files => L10nKey::SearchTabFiles,
            SearchTab::Actions => L10nKey::SearchTabActions,
            SearchTab::Terminals => L10nKey::SearchTabTerminals,
            SearchTab::Sessions => L10nKey::SearchTabSessions,
            SearchTab::Hosts => L10nKey::SearchTabHosts,
            SearchTab::Symbols => L10nKey::SearchTabSymbols,
            SearchTab::Locations => L10nKey::SearchTabLocations,
        })
    }

    /// A tab outside the window's row. It never sits under that row with
    /// nothing lit, and Tab never trades it for the terminals: the editor's
    /// tabs get their own row (`EDITOR_ORDER`), and a language server's places
    /// stand alone under their name.
    pub(crate) fn stands_alone(self) -> bool {
        !Self::ORDER.contains(&self)
    }

    pub(crate) fn placeholder(self) -> &'static str {
        t(match self {
            SearchTab::All => L10nKey::SearchPlaceholderAll,
            SearchTab::Files => L10nKey::SearchPlaceholderFiles,
            SearchTab::Actions => L10nKey::SearchPlaceholderActions,
            SearchTab::Terminals => L10nKey::SearchPlaceholderTerminals,
            SearchTab::Sessions => L10nKey::SearchPlaceholderSessions,
            SearchTab::Hosts => L10nKey::SearchPlaceholderHosts,
            SearchTab::Symbols => L10nKey::SearchPlaceholderSymbols,
            SearchTab::Locations => L10nKey::SearchPlaceholderLocations,
        })
    }

    /// The neighbouring tab, wrapping at the ends.
    pub(crate) fn step(self, forward: bool) -> SearchTab {
        let n = Self::ORDER.len();
        let i = Self::ORDER.iter().position(|t| *t == self).unwrap_or(0);
        Self::ORDER[if forward {
            (i + 1) % n
        } else {
            (i + n - 1) % n
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_steps_wrap_both_ways() {
        assert_eq!(SearchTab::All.step(true), SearchTab::Terminals);
        assert_eq!(SearchTab::Hosts.step(true), SearchTab::Actions);
        assert_eq!(SearchTab::Actions.step(true), SearchTab::All);
        assert_eq!(SearchTab::All.step(false), SearchTab::Actions);
        assert_eq!(SearchTab::Terminals.step(false), SearchTab::All);
        assert_eq!(SearchTab::Terminals.step(true), SearchTab::Sessions);
    }

    #[test]
    fn files_is_reached_by_its_chord_not_by_the_row() {
        assert!(!SearchTab::ORDER.contains(&SearchTab::Files));
        assert!(!SearchTab::ORDER.contains(&SearchTab::Symbols));
        // Reached by a chord, they stand alone rather than sit under a row
        // with nothing in it lit.
        for tab in [SearchTab::Files, SearchTab::Symbols, SearchTab::Locations] {
            assert!(tab.stands_alone());
        }
        assert!(SearchTab::ORDER.iter().all(|tab| !tab.stands_alone()));
    }
}

//! Which files each tab had open in the editor, kept across restarts.
//!
//! The tab tree itself lives in the daemon, and the daemon has no business
//! knowing about editor buffers — a file open in the GUI is not something a
//! shell or a remote server can act on. So this is the GUI's own record, keyed
//! by the tree's [`TabId`], which is stable across restarts: the tab that comes
//! back after a relaunch is the same tab, and gets its files back.
//!
//! One store per process, shared by every window, so two windows writing their
//! own tabs never race each other over the file.

use std::collections::HashMap;
use std::path::PathBuf;

use gpui::{App, Global};
use serde::{Deserialize, Serialize};
use tty7_core::core::machine::TabId;

const FILE: &str = "editor-sessions.json";

/// The most tabs remembered. A tab closed in some other client never tells
/// this one, so its record can only age out.
const MAX_TABS: usize = 256;

const WRITE_DELAY: std::time::Duration = std::time::Duration::from_millis(500);

/// One tab's editor, as it was last seen. `files` and `active` are the left
/// group's — the only group, unless the editor was split.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TabEditor {
    pub(crate) files: Vec<PathBuf>,
    #[serde(default)]
    pub(crate) active: usize,
    #[serde(default)]
    pub(crate) visible: bool,
    /// The right group of a split editor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) split: Option<SplitEditor>,
}

/// The right-hand group of a split editor.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SplitEditor {
    pub(crate) files: Vec<PathBuf>,
    #[serde(default)]
    pub(crate) active: usize,
    /// Whether this group, not the left one, had the focus.
    #[serde(default)]
    pub(crate) focused: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Entry {
    #[serde(flatten)]
    state: TabEditor,
    /// When this entry last changed, for choosing what to forget first.
    #[serde(default)]
    touched: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Doc {
    #[serde(default)]
    tabs: HashMap<TabId, Entry>,
}

#[derive(Default)]
struct EditorSessionStore {
    doc: Doc,
    write_scheduled: bool,
}

impl Global for EditorSessionStore {}

fn store(cx: &mut App) -> &mut EditorSessionStore {
    if cx.try_global::<EditorSessionStore>().is_none() {
        cx.set_global(EditorSessionStore {
            doc: load(),
            write_scheduled: false,
        });
    }
    cx.global_mut::<EditorSessionStore>()
}

fn load() -> Doc {
    // A test builds windows like any other run, and must not read — let alone
    // rewrite — the editor state of the person running the suite.
    if cfg!(test) {
        return Doc::default();
    }
    let Some(path) = crate::core::config::config_path(FILE) else {
        return Doc::default();
    };
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            log::warn!(
                "editor sessions: ignoring unreadable {}: {e}",
                path.display()
            );
            Doc::default()
        }),
        Err(_) => Doc::default(),
    }
}

/// What the tab had open, if anything was recorded for it.
pub(crate) fn get(cx: &mut App, tab: TabId) -> Option<TabEditor> {
    store(cx).doc.tabs.get(&tab).map(|e| e.state.clone())
}

/// Records a tab's editor. Writing is deferred and coalesced: this is called
/// whenever the set of open files changes, which a burst of opens makes often.
pub(crate) fn put(cx: &mut App, tab: TabId, state: TabEditor) {
    let s = store(cx);
    if s.doc.tabs.get(&tab).is_some_and(|e| e.state == state) {
        return;
    }
    s.doc.tabs.insert(
        tab,
        Entry {
            state,
            touched: now_secs(),
        },
    );
    prune(&mut s.doc);
    schedule_write(cx);
}

/// Forgets a tab that was closed.
pub(crate) fn remove(cx: &mut App, tab: TabId) {
    if store(cx).doc.tabs.remove(&tab).is_some() {
        schedule_write(cx);
    }
}

fn prune(doc: &mut Doc) {
    if doc.tabs.len() <= MAX_TABS {
        return;
    }
    let mut by_age: Vec<(u64, TabId)> = doc.tabs.iter().map(|(id, e)| (e.touched, *id)).collect();
    by_age.sort_by_key(|(touched, _)| *touched);
    for (_, id) in by_age.into_iter().take(doc.tabs.len() - MAX_TABS) {
        doc.tabs.remove(&id);
    }
}

fn schedule_write(cx: &mut App) {
    if cfg!(test) {
        return;
    }
    let s = store(cx);
    if s.write_scheduled {
        return;
    }
    s.write_scheduled = true;
    cx.spawn(async move |cx| {
        cx.background_executor().timer(WRITE_DELAY).await;
        let json = cx.update(|cx| {
            let s = cx.global_mut::<EditorSessionStore>();
            s.write_scheduled = false;
            serde_json::to_vec_pretty(&s.doc)
        });
        let json = match json {
            Ok(json) => json,
            Err(e) => {
                log::warn!("editor sessions: could not serialize: {e}");
                return;
            }
        };
        cx.background_executor()
            .spawn(async move {
                let Some(path) = crate::core::config::config_path(FILE) else {
                    return;
                };
                if let Err(e) = crate::core::config::write_atomic(&path, &json) {
                    log::warn!("editor sessions: could not write {}: {e}", path.display());
                }
            })
            .await;
    })
    .detach();
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_document_round_trips_with_tab_ids_as_keys() {
        let mut doc = Doc::default();
        let id = TabId::new();
        doc.tabs.insert(
            id,
            Entry {
                state: TabEditor {
                    files: vec![PathBuf::from("/src/main.rs"), PathBuf::from("/README.md")],
                    active: 1,
                    visible: true,
                    split: Some(SplitEditor {
                        files: vec![PathBuf::from("/src/lib.rs")],
                        active: 0,
                        focused: true,
                    }),
                },
                touched: 7,
            },
        );
        let json = serde_json::to_string(&doc).unwrap();
        let back: Doc = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tabs[&id].state, doc.tabs[&id].state);
    }

    #[test]
    fn a_record_from_before_the_split_still_reads() {
        let old = r#"{"tabs": {}}"#;
        assert!(serde_json::from_str::<Doc>(old).is_ok());
        let state: TabEditor =
            serde_json::from_str(r#"{"files": ["/a.rs"], "active": 0, "visible": true}"#).unwrap();
        assert_eq!(state.split, None);
        assert!(!serde_json::to_string(&state).unwrap().contains("split"));
    }

    #[test]
    fn the_oldest_tabs_are_forgotten_first() {
        let mut doc = Doc::default();
        let mut ids = Vec::new();
        for touched in 0..(MAX_TABS as u64 + 3) {
            let id = TabId::new();
            ids.push(id);
            doc.tabs.insert(
                id,
                Entry {
                    state: TabEditor::default(),
                    touched,
                },
            );
        }
        prune(&mut doc);
        assert_eq!(doc.tabs.len(), MAX_TABS);
        for old in &ids[..3] {
            assert!(!doc.tabs.contains_key(old));
        }
        assert!(doc.tabs.contains_key(ids.last().unwrap()));
    }
}

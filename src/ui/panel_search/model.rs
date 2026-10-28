//! What the Search tab knows, apart from how it is drawn: which search is
//! current, what came back for it, and how the hits fall into files.
//!
//! Kept free of gpui so every rule here — above all "a stale result never
//! lands" — is a plain unit test.

use std::io;
use std::path::{Path, PathBuf};

use tty59_core::host::{ContentHit, ContentQuery, ContentResults};

use crate::ui::host_ops::HostId;

/// One search, fully named: the same key twice is the same search, and a
/// search is only ever re-run when its key changes or someone asks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SearchKey {
    pub(crate) host: HostId,
    pub(crate) roots: Vec<PathBuf>,
    pub(crate) query: ContentQuery,
}

/// How a search came back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Found(ContentResults),
    /// The pattern did not compile. Carries the parser's explanation.
    BadPattern(String),
    /// The host cannot search contents at all — an older `tty59-server`.
    ServerTooOld,
    Failed(String),
}

impl Outcome {
    pub(crate) fn from_result(found: io::Result<ContentResults>) -> Outcome {
        match found {
            Ok(found) => Outcome::Found(found),
            Err(e) => match e.kind() {
                io::ErrorKind::InvalidInput => Outcome::BadPattern(e.to_string()),
                io::ErrorKind::Unsupported => Outcome::ServerTooOld,
                _ => Outcome::Failed(e.to_string()),
            },
        }
    }
}

/// The hits of one file, in the order the search met them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileGroup {
    pub(crate) path: PathBuf,
    /// The file's name.
    pub(crate) name: String,
    /// The directory it is in, relative to the root it was found under and
    /// spelled with `/` — empty for a file at the top of a root.
    pub(crate) dir: String,
    pub(crate) hits: Vec<ContentHit>,
}

/// What has landed, with the key it answers.
#[derive(Clone, Debug)]
pub(crate) struct Landed {
    pub(crate) key: SearchKey,
    pub(crate) outcome: Outcome,
    pub(crate) groups: Vec<FileGroup>,
}

/// The search lifecycle, with a generation counter so that only the answer to
/// the search asked for last is ever shown.
#[derive(Default)]
pub(crate) struct SearchRun {
    generation: u64,
    /// The search asked for last, whether or not it has come back.
    wanted: Option<SearchKey>,
    running: bool,
    landed: Option<Landed>,
}

impl SearchRun {
    /// Point the run at `key`. `Some(generation)` means a search has to be
    /// started for it; `None` means there is nothing new to do, either
    /// because `key` is what is already wanted or because there is nothing to
    /// search (`key` is `None`), in which case what was shown goes.
    pub(crate) fn retarget(&mut self, key: Option<SearchKey>) -> Option<u64> {
        if self.wanted == key {
            return None;
        }
        self.generation += 1;
        self.wanted = key;
        match self.wanted {
            Some(_) => {
                self.running = true;
                Some(self.generation)
            }
            None => {
                self.running = false;
                self.landed = None;
                None
            }
        }
    }

    /// Run the wanted search again — Enter in the field. The results on
    /// screen stay until the new ones replace them.
    pub(crate) fn refresh(&mut self) -> Option<(u64, SearchKey)> {
        let key = self.wanted.clone()?;
        self.generation += 1;
        self.running = true;
        Some((self.generation, key))
    }

    /// Whether `generation` is still the search being waited for.
    pub(crate) fn is_current(&self, generation: u64) -> bool {
        self.generation == generation
    }

    /// Take an answer. One for any search but the last asked is dropped, and
    /// `false` says so.
    pub(crate) fn accept(&mut self, generation: u64, outcome: Outcome) -> bool {
        if generation != self.generation {
            return false;
        }
        let Some(key) = self.wanted.clone() else {
            return false;
        };
        let groups = match &outcome {
            Outcome::Found(found) => group_hits(&found.hits, &key.roots),
            _ => Vec::new(),
        };
        self.running = false;
        self.landed = Some(Landed {
            key,
            outcome,
            groups,
        });
        true
    }

    pub(crate) fn running(&self) -> bool {
        self.running
    }

    pub(crate) fn wanted(&self) -> Option<&SearchKey> {
        self.wanted.as_ref()
    }

    pub(crate) fn landed(&self) -> Option<&Landed> {
        self.landed.as_ref()
    }
}

/// Gather hits into one group per file, files in the order first met.
pub(crate) fn group_hits(hits: &[ContentHit], roots: &[PathBuf]) -> Vec<FileGroup> {
    let mut groups: Vec<FileGroup> = Vec::new();
    let mut index: std::collections::HashMap<&Path, usize> = std::collections::HashMap::new();
    for hit in hits {
        let at = *index.entry(hit.path.as_path()).or_insert_with(|| {
            let (name, dir) = split_relative(&hit.path, roots);
            groups.push(FileGroup {
                path: hit.path.clone(),
                name,
                dir,
                hits: Vec::new(),
            });
            groups.len() - 1
        });
        groups[at].hits.push(hit.clone());
    }
    groups
}

/// A hit's path as the list shows it: the file name, and the directory it is
/// in below the root it was found under. Several roots each keep their own
/// name in front, or two `src/` would read as one.
///
/// Done on the text, not with `Path::strip_prefix`: a remote host's paths are
/// not this machine's, and a Windows peer's `C:\repo\src` has no components a
/// macOS `Path` would recognise.
pub(crate) fn split_relative(path: &Path, roots: &[PathBuf]) -> (String, String) {
    let full = path.to_string_lossy();
    let is_sep = |c: char| c == '/' || c == '\\';
    let root = roots
        .iter()
        .map(|r| r.to_string_lossy())
        .filter(|r| {
            full.strip_prefix(r.trim_end_matches(is_sep))
                .is_some_and(|rest| rest.starts_with(is_sep))
        })
        .max_by_key(|r| r.len());
    let (lead, rel) = match &root {
        Some(root) => {
            let rest = &full[root.trim_end_matches(is_sep).len()..];
            let lead = match roots.len() > 1 {
                true => root
                    .trim_end_matches(is_sep)
                    .rsplit(is_sep)
                    .next()
                    .unwrap_or_default()
                    .to_string(),
                false => String::new(),
            };
            (lead, rest.trim_start_matches(is_sep).to_string())
        }
        None => (String::new(), full.to_string()),
    };
    let rel = rel.replace('\\', "/");
    let (dir, name) = match rel.rsplit_once('/') {
        Some((dir, name)) => (dir.to_string(), name.to_string()),
        None => (String::new(), rel),
    };
    let dir = match (lead.is_empty(), dir.is_empty()) {
        (true, _) => dir,
        (false, true) => lead,
        (false, false) => format!("{lead}/{dir}"),
    };
    (name, dir)
}

/// Every hit in `groups`, counted.
pub(crate) fn hit_count(groups: &[FileGroup]) -> usize {
    groups.iter().map(|g| g.hits.len()).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tty59_core::host::ContentRange;

    fn hit(path: &str, line: u32) -> ContentHit {
        ContentHit {
            path: PathBuf::from(path),
            line,
            column: 1,
            text: "x".into(),
            ranges: vec![ContentRange { start: 0, end: 1 }],
        }
    }

    fn key(pattern: &str) -> SearchKey {
        SearchKey {
            host: HostId::LOCAL,
            roots: vec![PathBuf::from("/r")],
            query: ContentQuery {
                pattern: pattern.into(),
                ..ContentQuery::default()
            },
        }
    }

    fn found(hits: Vec<ContentHit>, truncated: bool) -> Outcome {
        Outcome::Found(ContentResults {
            hits,
            truncated,
            files_searched: 1,
        })
    }

    #[test]
    fn the_same_search_is_not_started_twice() {
        let mut run = SearchRun::default();
        let first = run.retarget(Some(key("a"))).expect("a new search runs");
        assert!(run.running());
        assert_eq!(run.retarget(Some(key("a"))), None);
        assert!(run.is_current(first));
    }

    #[test]
    fn a_stale_answer_never_lands() {
        let mut run = SearchRun::default();
        let old = run.retarget(Some(key("a"))).unwrap();
        let new = run.retarget(Some(key("ab"))).unwrap();
        assert!(!run.accept(old, found(vec![hit("/r/a.rs", 1)], false)));
        assert!(run.landed().is_none(), "the old answer is dropped");
        assert!(run.running(), "and the new one is still awaited");

        assert!(run.accept(new, found(vec![], false)));
        let landed = run.landed().unwrap();
        assert_eq!(landed.key, key("ab"));
        assert!(!run.running());
    }

    #[test]
    fn a_refresh_outranks_the_search_it_repeats() {
        let mut run = SearchRun::default();
        let first = run.retarget(Some(key("a"))).unwrap();
        let (again, k) = run.refresh().unwrap();
        assert_eq!(k, key("a"));
        assert!(!run.accept(first, found(vec![], false)));
        assert!(run.accept(again, found(vec![], false)));
    }

    #[test]
    fn nothing_to_search_clears_what_was_shown() {
        let mut run = SearchRun::default();
        let g = run.retarget(Some(key("a"))).unwrap();
        run.accept(g, found(vec![hit("/r/a.rs", 1)], false));
        assert_eq!(run.retarget(None), None);
        assert!(run.landed().is_none());
        assert!(!run.running());
        assert_eq!(run.refresh(), None, "there is nothing to refresh");
    }

    #[test]
    fn an_answer_after_clearing_does_not_bring_results_back() {
        let mut run = SearchRun::default();
        let g = run.retarget(Some(key("a"))).unwrap();
        run.retarget(None);
        assert!(!run.accept(g, found(vec![hit("/r/a.rs", 1)], false)));
        assert!(run.landed().is_none());
    }

    #[test]
    fn hits_group_by_file_in_the_order_first_met() {
        let hits = vec![
            hit("/r/src/b.rs", 3),
            hit("/r/src/b.rs", 9),
            hit("/r/a.rs", 1),
            hit("/r/src/b.rs", 12),
        ];
        let groups = group_hits(&hits, &[PathBuf::from("/r")]);
        let shape: Vec<(&str, &str, Vec<u32>)> = groups
            .iter()
            .map(|g| {
                (
                    g.name.as_str(),
                    g.dir.as_str(),
                    g.hits.iter().map(|h| h.line).collect(),
                )
            })
            .collect();
        assert_eq!(
            shape,
            vec![("b.rs", "src", vec![3, 9, 12]), ("a.rs", "", vec![1])]
        );
        assert_eq!(hit_count(&groups), 4);
    }

    #[test]
    fn a_landed_search_keeps_its_truncation() {
        let mut run = SearchRun::default();
        let g = run.retarget(Some(key("x"))).unwrap();
        run.accept(g, found(vec![hit("/r/a.rs", 1)], true));
        match &run.landed().unwrap().outcome {
            Outcome::Found(f) => assert!(f.truncated),
            other => panic!("{other:?}"),
        }
        assert_eq!(run.landed().unwrap().groups.len(), 1);
    }

    #[test]
    fn relative_paths_come_from_the_innermost_root() {
        let roots = [PathBuf::from("/r")];
        assert_eq!(
            split_relative(Path::new("/r/src/ui/app.rs"), &roots),
            ("app.rs".into(), "src/ui".into())
        );
        assert_eq!(
            split_relative(Path::new("/r/top.rs"), &roots),
            ("top.rs".into(), String::new())
        );
        // `/rx` is not under `/r`.
        assert_eq!(
            split_relative(Path::new("/rx/a.rs"), &roots),
            ("a.rs".into(), "/rx".into())
        );
    }

    #[test]
    fn several_roots_keep_their_names() {
        let roots = [PathBuf::from("/w/api"), PathBuf::from("/w/web/")];
        assert_eq!(
            split_relative(Path::new("/w/api/src/a.rs"), &roots),
            ("a.rs".into(), "api/src".into())
        );
        assert_eq!(
            split_relative(Path::new("/w/web/index.ts"), &roots),
            ("index.ts".into(), "web".into())
        );
    }

    #[test]
    fn a_windows_peers_paths_split_on_backslashes() {
        let roots = [PathBuf::from(r"C:\repo")];
        assert_eq!(
            split_relative(Path::new(r"C:\repo\src\main.rs"), &roots),
            ("main.rs".into(), "src".into())
        );
    }

    #[test]
    fn errors_are_sorted_into_what_the_panel_can_say() {
        let e = |kind| Err(io::Error::new(kind, "why"));
        assert_eq!(
            Outcome::from_result(e(io::ErrorKind::InvalidInput)),
            Outcome::BadPattern("why".into())
        );
        assert_eq!(
            Outcome::from_result(e(io::ErrorKind::Unsupported)),
            Outcome::ServerTooOld
        );
        assert_eq!(
            Outcome::from_result(e(io::ErrorKind::NotFound)),
            Outcome::Failed("why".into())
        );
    }
}

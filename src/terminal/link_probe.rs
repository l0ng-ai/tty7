//! Whether a path a pane printed actually exists — for panes that cannot
//! answer that question on the spot.
//!
//! A local pane resolves a file link straight out of the filesystem while the
//! mouse event is still on the stack. A pane whose paths live on another
//! machine cannot: the answer costs a round trip, and the click that wanted it
//! is long gone by the time it lands. So a remote pane keeps this cache
//! instead. A lookup either finds a recorded answer or reports
//! [`Probe::Unknown`](super::search::Probe::Unknown) and remembers the path as
//! wanted; the view then asks the host once for everything wanted and files
//! the replies here, so the *next* hover — a mouse-move away — is a hit.
//!
//! Answers are kept per host, because two hosts can disagree about the same
//! absolute path, and dropped whenever the pane's host changes underneath
//! them.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::ui::host_ops::HostId;

use super::search::Probe;

/// What a host said about one path. A directory is kept apart from a file
/// because a token carrying a line number can only be answered by a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Existence {
    File,
    Dir,
    Missing,
}

impl Existence {
    fn answers(self, require_file: bool) -> Probe {
        match (self, require_file) {
            (Existence::File, _) => Probe::Hit { is_dir: false },
            (Existence::Dir, false) => Probe::Hit { is_dir: true },
            (Existence::Dir, true) | (Existence::Missing, _) => Probe::Miss,
        }
    }
}

/// How many answers one pane keeps. Every hover over a path-shaped word adds
/// at most a handful, and the whole point is to answer the *next* mouse event,
/// so the working set is tiny; the cap only exists so a pane printing
/// thousands of distinct paths cannot grow this without bound.
const MAX_ANSWERS: usize = 512;

#[derive(Default)]
pub(super) struct LinkProbeCache {
    host: Option<HostId>,
    answers: HashMap<PathBuf, Existence>,
    /// Paths a lookup wanted and could not answer. Drained by the view, which
    /// turns them into one host call.
    wanted: HashSet<PathBuf>,
    /// Paths already out with the host, so a hover repeated every mouse-move
    /// asks once rather than once per frame.
    in_flight: HashSet<PathBuf>,
}

impl LinkProbeCache {
    /// Points the cache at `host`, clearing it if that is not the host the
    /// answers came from. Answers about one machine say nothing about another.
    pub fn retarget(&mut self, host: HostId) {
        if self.host == Some(host) {
            return;
        }
        self.host = Some(host);
        self.answers.clear();
        self.wanted.clear();
        self.in_flight.clear();
    }

    /// The cached answer for `path`, recording it as wanted when there is none.
    pub fn probe(&mut self, path: &Path, require_file: bool) -> Probe {
        if let Some(known) = self.answers.get(path) {
            return known.answers(require_file);
        }
        if !self.in_flight.contains(path) {
            self.wanted.insert(path.to_path_buf());
        }
        Probe::Unknown
    }

    /// The paths to ask the host about, moved into the in-flight set so the
    /// next lookup does not ask for them again.
    pub fn take_wanted(&mut self) -> Vec<PathBuf> {
        let wanted: Vec<PathBuf> = self.wanted.drain().collect();
        self.in_flight.extend(wanted.iter().cloned());
        wanted
    }

    /// Files what the host said. Returns whether anything the cache did not
    /// already know came back — a caller that re-resolves on every answer
    /// would otherwise repaint for replies that change nothing.
    pub fn land(&mut self, answers: Vec<(PathBuf, Existence)>) -> bool {
        let mut news = false;
        for (path, existence) in answers {
            self.in_flight.remove(&path);
            news |= self.answers.insert(path, existence) != Some(existence);
        }
        // Cheaper than tracking use order, and correct for the same reason the
        // cap is generous: what a hover needs was written microseconds ago, so
        // starting over costs one more round trip at worst.
        if self.answers.len() > MAX_ANSWERS {
            self.answers.clear();
        }
        news
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local() -> HostId {
        HostId::LOCAL
    }

    #[test]
    fn an_unknown_path_is_asked_for_once() {
        let mut cache = LinkProbeCache::default();
        cache.retarget(local());

        assert_eq!(cache.probe(Path::new("/a/b.rs"), true), Probe::Unknown);
        assert_eq!(cache.take_wanted(), vec![PathBuf::from("/a/b.rs")]);

        // Still unknown, but already out with the host — asking again would
        // put one call per frame on the wire for as long as the mouse rests.
        assert_eq!(cache.probe(Path::new("/a/b.rs"), true), Probe::Unknown);
        assert!(cache.take_wanted().is_empty());
    }

    #[test]
    fn a_directory_cannot_answer_a_token_carrying_a_line_number() {
        let mut cache = LinkProbeCache::default();
        cache.retarget(local());
        cache.land(vec![(PathBuf::from("/a/logs"), Existence::Dir)]);

        assert_eq!(
            cache.probe(Path::new("/a/logs"), false),
            Probe::Hit { is_dir: true }
        );
        assert_eq!(cache.probe(Path::new("/a/logs"), true), Probe::Miss);
    }

    #[test]
    fn a_miss_is_an_answer_and_is_not_asked_for_again() {
        let mut cache = LinkProbeCache::default();
        cache.retarget(local());
        cache.land(vec![(PathBuf::from("/a/nope"), Existence::Missing)]);

        assert_eq!(cache.probe(Path::new("/a/nope"), false), Probe::Miss);
        assert!(cache.take_wanted().is_empty());
    }

    #[test]
    fn landing_reports_only_answers_that_change_something() {
        let mut cache = LinkProbeCache::default();
        cache.retarget(local());

        assert!(cache.land(vec![(PathBuf::from("/a/b.rs"), Existence::File)]));
        assert!(!cache.land(vec![(PathBuf::from("/a/b.rs"), Existence::File)]));
        assert!(cache.land(vec![(PathBuf::from("/a/b.rs"), Existence::Missing)]));
    }

    #[test]
    fn answers_do_not_survive_a_change_of_host() {
        let mut cache = LinkProbeCache::default();
        cache.retarget(local());
        cache.land(vec![(PathBuf::from("/etc/hosts"), Existence::File)]);
        assert_eq!(
            cache.probe(Path::new("/etc/hosts"), false),
            Probe::Hit { is_dir: false }
        );

        cache.retarget(HostId::from_connection_key("ssh-direct:me@box:22"));
        assert_eq!(cache.probe(Path::new("/etc/hosts"), false), Probe::Unknown);
    }
}

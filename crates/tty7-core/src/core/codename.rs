//! Two-word names for things a person did not bother to name — worktrees and
//! workspaces. "quiet-otter" beats "Untitled 3" at telling two of them apart in
//! a list, and beats a directory name at surviving a `cd`.

const ADJECTIVES: [&str; 24] = [
    "quiet", "amber", "bold", "calm", "cedar", "coral", "dusky", "early", "fable", "gold", "hazel",
    "ivory", "jade", "keen", "lunar", "mossy", "noble", "ochre", "pale", "rapid", "sunny", "tidal",
    "vivid", "wild",
];
const NOUNS: [&str; 24] = [
    "otter", "heron", "lynx", "wren", "fox", "elk", "crane", "finch", "gecko", "ibis", "koala",
    "llama", "marten", "newt", "osprey", "puffin", "quail", "raven", "seal", "tern", "urchin",
    "vole", "walrus", "yak",
];

/// A xorshift state, seeded off the clock and the pid.
pub struct Names(u64);

impl Default for Names {
    fn default() -> Self {
        Self::new()
    }
}

impl Names {
    pub fn new() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15);
        Names(nanos ^ ((std::process::id() as u64) << 32) | 1)
    }

    #[cfg(test)]
    pub fn seeded(seed: u64) -> Self {
        Names(seed | 1)
    }

    pub fn roll(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// One "adjective-noun", with no regard for what is already taken.
    pub fn candidate(&mut self) -> String {
        let a = ADJECTIVES[(self.roll() % ADJECTIVES.len() as u64) as usize];
        let n = NOUNS[(self.roll() % NOUNS.len() as u64) as usize];
        format!("{a}-{n}")
    }

    /// A name `taken` rejects nothing about. 576 pairs collide sooner than you
    /// would like, so after enough tries a number goes on the end and the
    /// search stops being able to fail.
    pub fn unique(&mut self, taken: impl Fn(&str) -> bool) -> String {
        let mut name = self.candidate();
        for attempt in 0..64 {
            if !taken(&name) {
                break;
            }
            name = if attempt < 32 {
                self.candidate()
            } else {
                format!("{}-{}", self.candidate(), self.roll() % 1000)
            };
        }
        name
    }
}

/// A name not already in `taken`.
pub fn unique(taken: impl Fn(&str) -> bool) -> String {
    Names::new().unique(taken)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_candidate_is_two_lowercase_words() {
        let name = Names::seeded(7).candidate();
        let (a, n) = name.split_once('-').expect("adjective-noun");
        assert!(ADJECTIVES.contains(&a), "{a} is not an adjective");
        assert!(NOUNS.contains(&n), "{n} is not a noun");
    }

    #[test]
    fn unique_walks_past_names_already_taken() {
        let first = Names::seeded(7).candidate();
        let picked = Names::seeded(7).unique(|n| n == first);
        assert_ne!(picked, first);
    }

    #[test]
    fn unique_still_answers_when_everything_is_taken() {
        // Every bare pair is refused, so it has to fall through to the
        // numbered form rather than spin or hand back a duplicate.
        let name = Names::seeded(7).unique(|n| n.matches('-').count() < 2);
        assert_eq!(name.matches('-').count(), 2, "{name} should carry a number");
    }
}

//! Per-command completion signatures — tty7's take on rich command
//! signatures (built on Fig's autocomplete specs).
//!
//! The data is generated offline from Fig's MIT-licensed spec corpus by
//! `scripts/fig-convert/convert.mjs`, which executes each compiled spec and
//! snapshots its *static* shape (subcommands, options, args, descriptions,
//! static generator `script`s) into `assets/completions/<cmd>.json`. This module
//! is only the runtime consumer: a serde model plus a per-command **lazy,
//! memoized registry** — a command's JSON is parsed the first time it's typed
//! and cached for the session.
//!
//! Specs are read from an on-disk `completions/` directory rather than embedded
//! in the binary, so the corpus can grow (or a user can drop in their own specs)
//! without a recompile and without bloating the executable. [`spec_source`]
//! resolves that directory across the shapes tty7 runs in — a packaged bundle,
//! an unpackaged binary, `cargo run`, and tests — plus an optional user override
//! under the config dir; see its docs for the search order. The lookup only ever
//! maps a bare command name to `<dir>/<cmd>.json`, so a typed token can't escape
//! the completions dir.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use serde::Deserialize;

/// A command's completion signature (the JSON root). Shares the `options` /
/// `args` / `subcommands` shape with [`Subcommand`] via the [`CmdNode`] trait so
/// the argv walk can treat the root and any nested subcommand uniformly.
#[derive(Debug, Deserialize)]
pub struct Signature {
    #[allow(dead_code)]
    pub name: String,
    #[allow(dead_code)]
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub options: Vec<Opt>,
    #[serde(default)]
    pub args: Vec<Arg>,
    #[serde(default)]
    pub subcommands: Vec<Subcommand>,
}

/// A subcommand node — the same fields as [`Signature`] but carrying its own
/// aliases (`names`) and a `hidden` flag we keep out of the menu.
#[derive(Debug, Deserialize)]
pub struct Subcommand {
    #[serde(default)]
    pub names: Vec<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub hidden: bool,
    /// A per-entry icon from the Fig spec: an emoji, a `fig://icon?type=…`
    /// template, or a `fig://template?…`. The menu renderer interprets it.
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub options: Vec<Opt>,
    #[serde(default)]
    pub args: Vec<Arg>,
    #[serde(default)]
    pub subcommands: Vec<Subcommand>,
}

/// A flag / option. `names` holds every spelling (`["-m", "--message"]`); a
/// non-empty `args` means the option takes a value.
#[derive(Debug, Deserialize)]
pub struct Opt {
    #[serde(default)]
    pub names: Vec<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub args: Vec<Arg>,
    #[allow(dead_code)]
    #[serde(default)]
    pub required: bool,
    #[allow(dead_code)]
    #[serde(default)]
    pub repeatable: bool,
    #[serde(default)]
    pub hidden: bool,
    /// Per-option icon from the Fig spec (see [`Subcommand::icon`]).
    #[serde(default)]
    pub icon: Option<String>,
}

impl Opt {
    /// Whether this option consumes a following value token.
    pub fn takes_arg(&self) -> bool {
        !self.args.is_empty()
    }
}

/// A positional / value argument. `template` mirrors Fig's `"filepaths"` /
/// `"folders"` (→ tty7's path completion); `suggestions` is a static candidate
/// list; `generators` holds only the *static* `script`s (dynamic value
/// completion — running them — is a later step, so they're unused for now).
#[derive(Debug, Deserialize)]
pub struct Arg {
    #[allow(dead_code)]
    #[serde(default)]
    pub name: Option<String>,
    #[allow(dead_code)]
    #[serde(default)]
    pub optional: bool,
    #[allow(dead_code)]
    #[serde(default)]
    pub variadic: bool,
    #[serde(default)]
    pub template: Vec<String>,
    #[serde(default)]
    pub suggestions: Vec<Suggestion>,
    #[allow(dead_code)]
    #[serde(default)]
    pub generators: Vec<Generator>,
}

impl Arg {
    /// Whether this arg wants filesystem completion (Fig `filepaths`/`folders`).
    pub fn wants_paths(&self) -> bool {
        self.template
            .iter()
            .any(|t| t == "filepaths" || t == "folders")
    }
}

/// A static value suggestion for an argument.
#[derive(Debug, Deserialize)]
pub struct Suggestion {
    #[serde(default)]
    pub names: Vec<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// Per-suggestion icon from the Fig spec (see [`Subcommand::icon`]).
    #[serde(default)]
    pub icon: Option<String>,
}

/// A dynamic-value generator, reduced to its static shell `script` (the JS
/// `postProcess` is dropped at conversion time; tty7 would default to
/// one-suggestion-per-line). Not executed yet — kept so the data is ready.
#[derive(Debug, Deserialize)]
pub struct Generator {
    #[allow(dead_code)]
    #[serde(default)]
    pub script: Vec<String>,
}

/// Uniform read access to a command node's children, so the argv walk in
/// `completion` can start at the [`Signature`] root and descend into
/// [`Subcommand`]s without special-casing.
pub trait CmdNode {
    fn subcommands(&self) -> &[Subcommand];
    fn options(&self) -> &[Opt];
    fn args(&self) -> &[Arg];

    /// The subcommand whose name/alias equals `token`, if any.
    fn find_subcommand(&self, token: &str) -> Option<&Subcommand> {
        self.subcommands()
            .iter()
            .find(|s| s.names.iter().any(|n| n == token))
    }

    /// The option matching a flag token (`--message`, `-m`); the token is
    /// compared after stripping any `=value` suffix.
    fn find_option(&self, token: &str) -> Option<&Opt> {
        let flag = token.split('=').next().unwrap_or(token);
        self.options()
            .iter()
            .find(|o| o.names.iter().any(|n| n == flag))
    }
}

impl CmdNode for Signature {
    fn subcommands(&self) -> &[Subcommand] {
        &self.subcommands
    }
    fn options(&self) -> &[Opt] {
        &self.options
    }
    fn args(&self) -> &[Arg] {
        &self.args
    }
}

impl CmdNode for Subcommand {
    fn subcommands(&self) -> &[Subcommand] {
        &self.subcommands
    }
    fn options(&self) -> &[Opt] {
        &self.options
    }
    fn args(&self) -> &[Arg] {
        &self.args
    }
}

/// The directories searched for `<cmd>.json`, most-specific first, resolved once.
///
/// Order (first hit wins, so earlier entries override later ones):
///   1. `$TTY7_COMPLETIONS_DIR` — explicit override for dev / testing.
///   2. `<config-dir>/completions` — user-supplied specs (mirrors how the rest of
///      tty7 lets `~/.config/tty7` override built-ins).
///   3. bundle/executable-relative — where each packaging script installs the
///      specs: `../Resources/completions` inside a macOS `.app`, or a
///      `completions/` dir beside the executable on Linux/Windows.
///   4. the in-tree `assets/completions` — the `cargo run` / test fallback,
///      baked in via `CARGO_MANIFEST_DIR` so an unpackaged run still finds specs.
fn spec_source() -> &'static [PathBuf] {
    static DIRS: OnceLock<Vec<PathBuf>> = OnceLock::new();
    DIRS.get_or_init(|| {
        let mut dirs = Vec::new();
        if let Some(over) = std::env::var_os("TTY7_COMPLETIONS_DIR") {
            dirs.push(PathBuf::from(over));
        }
        if let Some(cfg) = crate::core::config::config_dir_path() {
            dirs.push(cfg.join("completions"));
        }
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                dirs.push(dir.join("../Resources/completions")); // macOS .app
                dirs.push(dir.join("completions")); // Linux / Windows sibling
            }
        }
        dirs.push(PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/completions"
        )));
        dirs
    })
}

/// Read the raw JSON for `cmd` from the first [`spec_source`] dir that has it.
///
/// `cmd` maps to the bare filename `<cmd>.json`; anything that isn't a plain
/// command token (letters, digits, and `._+-`) is rejected up front so a typed
/// token can never contain a path separator or `..` and read outside the dir.
fn raw_spec(cmd: &str) -> Option<String> {
    if cmd.is_empty()
        || !cmd
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'-'))
    {
        return None;
    }
    let file = format!("{cmd}.json");
    spec_source()
        .iter()
        .find_map(|dir| std::fs::read_to_string(dir.join(&file)).ok())
}

/// The parse cache: `None` marks a command we've looked up and have no (or
/// unparseable) signature for, so a miss is memoized too.
type Registry = Mutex<HashMap<String, Option<Arc<Signature>>>>;

fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The signature for `cmd`, parsed lazily on first use and memoized (hit or
/// miss). Returns `None` for commands outside the embedded corpus or whose JSON
/// fails to parse — callers fall back to generic completion.
pub fn signature(cmd: &str) -> Option<Arc<Signature>> {
    // Fast path: return the memoized result (hit or miss) without touching disk.
    if let Some(cached) = registry().lock().unwrap().get(cmd) {
        return cached.clone();
    }
    // Read + parse off-lock so filesystem IO never blocks another lookup. A
    // concurrent miss may load the same spec twice; that's idempotent, and the
    // insert below just re-publishes the same value.
    let parsed = raw_spec(cmd).and_then(|raw| match serde_json::from_str::<Signature>(&raw) {
        Ok(sig) => Some(Arc::new(sig)),
        Err(e) => {
            log::warn!("failed to parse completion signature for {cmd}: {e}");
            None
        }
    });
    registry()
        .lock()
        .unwrap()
        .insert(cmd.to_string(), parsed.clone());
    parsed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_signature_parses_and_memoizes() {
        let sig = signature("git").expect("git spec on disk");
        assert_eq!(sig.name, "git");
        assert!(sig.subcommands.len() > 20, "git has many subcommands");
        // Second call returns the same cached Arc.
        let again = signature("git").unwrap();
        assert!(Arc::ptr_eq(&sig, &again));
    }

    #[test]
    fn docker_loadspec_grafted_compose() {
        let sig = signature("docker").expect("docker spec on disk");
        // `docker compose` was grafted from the docker-compose spec via loadSpec.
        let compose = sig
            .find_subcommand("compose")
            .expect("compose subcommand present");
        assert!(
            !compose.subcommands.is_empty(),
            "compose should carry grafted subcommands"
        );
    }

    #[test]
    fn git_commit_message_option_takes_arg() {
        let sig = signature("git").unwrap();
        let commit = sig.find_subcommand("commit").unwrap();
        let message = commit.find_option("--message").unwrap();
        assert!(message.names.iter().any(|n| n == "-m"));
        assert!(message.takes_arg());
    }

    #[test]
    fn unknown_command_has_no_signature() {
        assert!(signature("definitely-not-a-real-cmd-xyz").is_none());
    }

    /// Every spec that ships in-tree must parse into the serde model — a
    /// malformed one should fail here (at CI time) rather than silently
    /// degrading to generic completion on a user's machine.
    #[test]
    fn every_shipped_spec_parses() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/completions");
        let mut count = 0;
        for entry in std::fs::read_dir(dir).expect("completions dir exists") {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let raw = std::fs::read_to_string(&path).unwrap();
            serde_json::from_str::<Signature>(&raw)
                .unwrap_or_else(|e| panic!("{} failed to parse: {e}", path.display()));
            count += 1;
        }
        assert!(count >= 2, "expected the shipped corpus, found {count} specs");
    }

    /// A typed token that isn't a bare command name must never read a file —
    /// path separators and `..` are rejected before touching the filesystem.
    #[test]
    fn raw_spec_rejects_path_traversal() {
        assert!(raw_spec("git").is_some());
        assert!(raw_spec("../git").is_none());
        assert!(raw_spec("a/b").is_none());
        assert!(raw_spec("../../etc/passwd").is_none());
        assert!(raw_spec("").is_none());
    }
}

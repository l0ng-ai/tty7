//! `.tty7/setup`: the project's own "make this checkout runnable" step —
//! install dependencies, write a `.env.local`, seed a database.
//!
//! It is an executable in the repo, any language, run in the new worktree's
//! first pane before the agent starts, so its output is on screen and a
//! failure stops there instead of under an agent that cannot build. tty7 does
//! not run it anywhere else: the `Host` trait cannot run a command, and a
//! pane is where a person can see what a script did.
//!
//! It is code from the repo, so it runs only once someone has approved that
//! exact content ([`Setup::digest`]) for that repo; an edit asks again.
//!
//! The script is told where the main checkout and the worktree are, and
//! `TTY7_PORT` starts a block of ten ports the
//! worktree can use without colliding with its siblings — derived from its
//! path, so no allocator has to remember anything and a worktree recreated at
//! the same place gets the same ports back.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::core::cli_agent::CLIAgent;
use crate::core::shell_quote::quote_for_shell;
use crate::host::{Host, HostId, fnv1a64};

pub const SCRIPT: &[&str] = &[".tty7", "setup"];

const PORT_FIRST: u16 = 20_000;
const PORT_BLOCKS: u64 = 1_000;
pub const PORT_BLOCK: u16 = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setup {
    pub script: PathBuf,
    /// sha256 of the script, hex: what an approval is for.
    pub digest: String,
}

/// The worktree's setup script, if it has one.
pub fn find(host: &dyn Host, worktree: &Path) -> Option<Setup> {
    let script = SCRIPT
        .iter()
        .fold(worktree.to_path_buf(), |p, c| host.join(&p, c));
    let bytes = host.read_file(&script, 1 << 20).ok()?;
    Some(Setup {
        script,
        digest: hex(&Sha256::digest(&bytes)),
    })
}

/// What an approval is filed under: the repo, on the machine it lives on.
pub fn trust_key(host: HostId, main_root: &Path) -> String {
    format!("{}:{}", host.0, main_root.display())
}

/// The first of the worktree's [`PORT_BLOCK`] ports.
pub fn port_base(worktree: &Path) -> u16 {
    let h = fnv1a64(worktree.to_string_lossy().as_bytes());
    PORT_FIRST + (h % PORT_BLOCKS) as u16 * PORT_BLOCK
}

/// The variables the script runs with. `None` unless both paths are
/// absolute: a script doing `rm -rf "$TTY7_WORKTREE_PATH/dist"` must never
/// see an empty or relative value.
pub fn env(
    host: &dyn Host,
    main_root: &Path,
    worktree: &Path,
) -> Option<Vec<(&'static str, String)>> {
    if !host.is_absolute(main_root) || !host.is_absolute(worktree) {
        return None;
    }
    let name = worktree.file_name()?.to_string_lossy().into_owned();
    Some(vec![
        ("TTY7_ROOT_PATH", main_root.display().to_string()),
        ("TTY7_WORKTREE_PATH", worktree.display().to_string()),
        ("TTY7_WORKTREE_NAME", name),
        ("TTY7_PORT", port_base(worktree).to_string()),
    ])
}

/// For a repo without a setup script: the install its lockfile implies, as a
/// suggestion to show — never something run on the user's behalf.
pub fn hint(host: &dyn Host, worktree: &Path) -> Option<&'static str> {
    const LOCKS: &[(&str, &str)] = &[
        ("pnpm-lock.yaml", "pnpm install"),
        ("bun.lock", "bun install"),
        ("bun.lockb", "bun install"),
        ("yarn.lock", "yarn install"),
        ("package-lock.json", "npm ci"),
        ("uv.lock", "uv sync"),
        ("poetry.lock", "poetry install"),
        ("Gemfile.lock", "bundle install"),
        ("composer.lock", "composer install"),
        ("mix.lock", "mix deps.get"),
    ];
    LOCKS
        .iter()
        .find(|(file, _)| host.exists(&host.join(worktree, file)))
        .map(|(_, cmd)| *cmd)
}

/// The line a new worktree's first pane types: the setup script under its
/// variables, then the agent — chained with `&&`, so a failed setup stops
/// there, on screen, instead of under an agent that cannot build.
pub fn launch_line(
    setup: Option<(&Path, &[(&'static str, String)])>,
    agent: Option<String>,
) -> Option<String> {
    let q = |s: &str| quote_for_shell(s, None);
    let setup = setup.map(|(script, env)| {
        let mut parts = vec!["env".to_string()];
        parts.extend(env.iter().map(|(k, v)| format!("{k}={}", q(v))));
        parts.push(q(&script.to_string_lossy()));
        parts.join(" ")
    });
    match (setup, agent) {
        (Some(setup), Some(agent)) => Some(format!("{setup} && {agent}")),
        (setup, agent) => setup.or(agent),
    }
}

/// `agent`'s launch line (`overrides` is `Config::agent_launch`), opening on
/// `task` when the agent takes a first message.
pub fn agent_line(agent: CLIAgent, task: &str, overrides: &HashMap<String, String>) -> String {
    let mut line = agent.launch_command(overrides);
    let task = task.trim();
    if let Some(args) = (!task.is_empty())
        .then(|| agent.prompt_args(task))
        .flatten()
    {
        for arg in args {
            line.push(' ');
            line.push_str(&quote_for_shell(&arg, None));
        }
    }
    line
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ports_are_stable_blocks_inside_the_range() {
        let a = port_base(Path::new("/r/.tty7/worktrees/quiet-otter"));
        assert_eq!(a, port_base(Path::new("/r/.tty7/worktrees/quiet-otter")));
        assert_eq!((a - PORT_FIRST) % PORT_BLOCK, 0);
        assert!(a >= PORT_FIRST && a + PORT_BLOCK <= PORT_FIRST + 10_000);
        assert_ne!(a, port_base(Path::new("/r/.tty7/worktrees/amber-heron")));
    }

    #[cfg(unix)]
    #[test]
    fn launch_line_chains_setup_before_the_agent_and_quotes_values() {
        let env = vec![
            ("TTY7_ROOT_PATH", "/r/my repo".to_string()),
            ("TTY7_PORT", "20010".to_string()),
        ];
        let script = Path::new("/r/my repo/.tty7/worktrees/w/.tty7/setup");
        assert_eq!(
            launch_line(Some((script, &env)), Some("claude 'fix it'".into())).unwrap(),
            "env TTY7_ROOT_PATH='/r/my repo' TTY7_PORT=20010 \
             '/r/my repo/.tty7/worktrees/w/.tty7/setup' && claude 'fix it'"
        );
        assert_eq!(launch_line(None, Some("codex".into())).unwrap(), "codex");
        assert_eq!(launch_line(None, None), None);
    }

    #[cfg(unix)]
    #[test]
    fn agent_line_passes_the_task_only_to_agents_that_take_one() {
        let none = HashMap::new();
        assert_eq!(
            agent_line(CLIAgent::Claude, " it's done ", &none),
            r"claude 'it'\''s done'"
        );
        assert_eq!(agent_line(CLIAgent::Gemini, "go", &none), "gemini -i go");
        assert_eq!(agent_line(CLIAgent::Claude, "  ", &none), "claude");
        assert_eq!(agent_line(CLIAgent::Aider, "go", &none), "aider");
        let flags = HashMap::from([(
            "claude".to_string(),
            "claude --dangerously-skip-permissions".to_string(),
        )]);
        assert_eq!(
            agent_line(CLIAgent::Claude, "go", &flags),
            "claude --dangerously-skip-permissions go"
        );
    }

    #[test]
    fn env_refuses_relative_paths() {
        let h = crate::host::local::LocalHost::new();
        // Absolute on every platform: `/abs` is not, on Windows.
        let root = std::env::temp_dir();
        let wt = root.join(".tty7").join("worktrees").join("w");
        assert!(env(&*h, Path::new("rel"), &wt).is_none());
        assert!(env(&*h, &root, Path::new("")).is_none());
        let vars = env(&*h, &root, &wt).unwrap();
        assert_eq!(vars[2], ("TTY7_WORKTREE_NAME", "w".to_string()));
    }

    #[test]
    fn find_digests_the_script_and_hint_reads_lockfiles() {
        let dir = std::env::temp_dir().join(format!("tty7-setup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".tty7")).unwrap();
        let h = crate::host::local::LocalHost::new();
        assert!(find(&*h, &dir).is_none());
        assert_eq!(hint(&*h, &dir), None);
        std::fs::write(dir.join("pnpm-lock.yaml"), "").unwrap();
        assert_eq!(hint(&*h, &dir), Some("pnpm install"));
        std::fs::write(dir.join(".tty7/setup"), "#!/bin/sh\n").unwrap();
        let a = find(&*h, &dir).unwrap();
        assert_eq!(a.digest.len(), 64);
        std::fs::write(dir.join(".tty7/setup"), "#!/bin/sh\necho x\n").unwrap();
        assert_ne!(find(&*h, &dir).unwrap().digest, a.digest);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

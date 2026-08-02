//! The tty7 orchestration *skill*: a Claude Code skill file describing how a
//! primary agent delegates work to worker panes over the session CLI
//! (`tab new` / `send` / `wait` / `capture`), installed at
//! `~/.claude/skills/tty7-orchestration/SKILL.md`.
//!
//! A skill, deliberately not a global instruction. An earlier cut of this
//! feature appended guidance to `~/.claude/CLAUDE.md` / `~/.codex/AGENTS.md`,
//! which taxed every session's context window and — worse — encouraged *every*
//! agent to discover and orchestrate its neighbours. The common shape is
//! primary → workers: one agent owns decomposition, dispatch, waiting and
//! aggregation, and the workers just do bounded tasks. A skill fits that
//! exactly: only its one-line description rides in context until the user or
//! the primary agent explicitly reaches for it, and workers never see it.
//!
//! The file is wholly tty7-owned (marker inside, checked before any delete),
//! so install is a plain overwrite — also the version-refresh path — and
//! uninstall removes the file, never guessing at merged user edits.

use std::path::PathBuf;

/// Ownership marker. Uninstall refuses to delete a file without it, so a
/// hand-written skill that happens to share the directory name survives.
const MARKER: &str = "<!-- managed by tty7 (Settings → Agents); edits are overwritten -->";

const SKILL_DIR: &str = "tty7-orchestration";

/// The skill itself. The frontmatter description is what Claude Code matches
/// against a session's intent, so it names the *tasks* that should trigger it;
/// the body can afford real workflow detail because it only loads on use.
const SKILL: &str = "\
---
name: tty7-orchestration
description: Delegate work to other coding agents running in tty7 terminal panes — spawn a worker pane, send it a prompt, wait until it needs input or finishes, and capture its output. Use when asked to parallelize work across agents, run an agent team, or drive another terminal session in tty7.
---

<!-- managed by tty7 (Settings → Agents); edits are overwritten -->

# Orchestrating tty7 sessions

You are the primary agent; panes you create are workers. Keep workers
bounded: give each a self-contained task, and keep decomposition, waiting,
and aggregation here. Do not hand workers orchestration duties — a worker
that finishes its task and stops is what keeps an agent team debuggable.

Prerequisites: you are inside tty7 (the `TTY7` env var is set) and the
`tty7` CLI is on PATH. `%N` addresses a pane by id; `$TTY7_PANE` is your own
pane. Every verb takes `--json`.

## The delegation loop

1. Create a worker pane: `tty7 tab new --cwd DIR` — prints the pane id (`%N`)
2. Start the worker: `tty7 send %N 'claude \"one bounded task\"' --enter`
   - interactive, not `claude -p`: headless print mode never stops to ask,
     so the `waiting` state this loop turns on would never arrive
3. Sleep until it needs you:
   `tty7 wait %N --until waiting,done --changed --timeout 600`
   - exit 0: the JSON report names the matched state, with the agent's
     message and native session id
   - exit 124: still working — wait again, or look in on it
   - exit 1 with `\"status\": \"exit\"`: the worker died; do not wait again
4. If it is *waiting* (a permission prompt or question), read and answer it:
   `tty7 capture %N --plain`, then `tty7 send %N 'y' --enter` (or whatever
   the prompt asks) — then go back to step 3
5. When *done*, collect the result: `tty7 capture %N --plain`
6. Clean up: `tty7 pane close %N`

Always pass `--changed` when you wait after sending something. The status the
server keeps is a level, not an event: `done` stands until the next turn
begins and `waiting` stands until the agent moves, so a plain `wait` issued
right after a `send` answers with the *previous* turn's state before the
worker has even read your input. `--changed` ignores the state the pane was
already in. Without it, check `\"stale\": true` in the JSON before trusting a
wake-up.

Run workers in parallel by repeating steps 1–2, then waiting on each pane.
`tty7 ls` shows every workspace, tab and pane; `tty7 agents` shows every
agent and its status at a glance.
";

/// The skill's install path: `~/.claude/skills/tty7-orchestration/SKILL.md`,
/// honoring the same `CLAUDE_CONFIG_DIR` override the hooks installer honors.
fn skill_path() -> Option<PathBuf> {
    let base = if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty()) {
        PathBuf::from(dir)
    } else {
        home_dir()?.join(".claude")
    };
    Some(base.join("skills").join(SKILL_DIR).join("SKILL.md"))
}

fn home_dir() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
    #[cfg(not(unix))]
    {
        std::env::var_os("USERPROFILE").map(PathBuf::from)
    }
}

/// Install (or refresh) the skill. A plain overwrite: the file is wholly
/// tty7-owned, and this doubling as the version-refresh path is the point.
pub fn install() -> anyhow::Result<String> {
    let path = skill_path().ok_or_else(|| anyhow::anyhow!("cannot resolve home directory"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    crate::core::config::write_atomic(&path, SKILL.as_bytes())?;
    Ok("Installed".to_string())
}

/// Remove the skill — but only a file carrying the ownership marker, so a
/// user's own `tty7-orchestration` skill is never deleted by tty7. The
/// directory goes too once empty; an empty skill dir would read as a broken
/// skill in Claude Code's listing.
pub fn uninstall() -> anyhow::Result<String> {
    let path = skill_path().ok_or_else(|| anyhow::anyhow!("cannot resolve home directory"))?;
    match std::fs::read_to_string(&path) {
        Ok(content) if content.contains(MARKER) => {
            std::fs::remove_file(&path)?;
            if let Some(dir) = path.parent() {
                let _ = std::fs::remove_dir(dir); // fails non-empty; that's the guard
            }
            Ok("Removed".to_string())
        }
        Ok(_) => anyhow::bail!(
            "{} exists but was not installed by tty7 — not touching it",
            path.display()
        ),
        Err(_) => Ok("Removed".to_string()),
    }
}

/// Whether the tty7-owned skill is currently installed.
pub fn installed() -> bool {
    skill_path().is_some_and(|p| {
        std::fs::read_to_string(p)
            .map(|s| s.contains(MARKER))
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The skill text itself is load-bearing: the frontmatter must parse as a
    /// skill (name + description) and the body must carry the ownership
    /// marker `uninstall` keys on.
    #[test]
    fn skill_content_is_well_formed() {
        assert!(SKILL.starts_with("---\nname: tty7-orchestration\n"));
        assert!(SKILL.contains("\ndescription: "));
        // Frontmatter is closed before the body starts.
        assert_eq!(SKILL.matches("\n---\n").count(), 1);
        assert!(SKILL.contains(MARKER));
        // The loop teaches the four primitives, not a stale verb set.
        for verb in ["tab new", "send %N", "wait %N", "capture %N", "pane close"] {
            assert!(SKILL.contains(verb), "skill body lost `{verb}`");
        }
        // The whole loop rests on `--changed`: without it a wait issued right
        // after a send answers with the previous turn's status.
        assert!(SKILL.contains("--changed"), "the loop lost --changed");
        // And the worker must be *launched* interactively — `claude -p` never
        // reaches the `waiting` state steps 3–4 are built on. The prose may
        // still name it; the command in step 2 may not start with it.
        assert!(
            !SKILL.contains("'claude -p"),
            "headless print mode cannot produce the `waiting` state this loop waits for"
        );
    }

    /// Owns `CLAUDE_CONFIG_DIR` and a scratch directory for the length of a
    /// test, and puts both back on the way out — including on a panic, which
    /// a plain tail cleanup would skip, leaving the var set for whatever runs
    /// next in this process.
    struct ScratchConfigDir(PathBuf);

    impl ScratchConfigDir {
        fn new(tag: &str) -> ScratchConfigDir {
            let dir =
                std::env::temp_dir().join(format!("tty7-skill-test-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            // SAFETY: test-scoped env mutation. `skill_path` is the only
            // reader of this var in this binary, and the guard restores it.
            unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", &dir) };
            ScratchConfigDir(dir)
        }
    }

    impl Drop for ScratchConfigDir {
        fn drop(&mut self) {
            // SAFETY: as above — undoing what `new` did.
            unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") };
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Install → installed → uninstall round-trips against a scratch
    /// `CLAUDE_CONFIG_DIR`; a foreign (marker-less) file is refused, not
    /// deleted. Env-var scoped: this test owns the var for its duration.
    #[test]
    fn install_roundtrip_and_foreign_file_safety() {
        let guard = ScratchConfigDir::new("roundtrip");
        let scratch = guard.0.clone();

        assert!(!installed());
        install().unwrap();
        assert!(installed());
        let path = scratch.join("skills").join(SKILL_DIR).join("SKILL.md");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), SKILL);

        // Re-install is the refresh path: same content, no error.
        install().unwrap();
        assert!(installed());

        uninstall().unwrap();
        assert!(!installed());
        assert!(!path.exists());
        assert!(!path.parent().unwrap().exists(), "empty skill dir lingers");

        // A user's own skill under our name must survive an uninstall.
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "---\nname: tty7-orchestration\n---\nmy own\n").unwrap();
        assert!(!installed(), "a foreign file is not a tty7 install");
        assert!(uninstall().is_err());
        assert!(path.exists(), "the user's file was deleted");
    }
}

//! The agent-coordination note: a short, marked section describing the
//! session CLI (`tty7 ls / spawn / send / wait / capture / kill`), installed
//! into the instruction files coding agents load on every session —
//! `~/.claude/CLAUDE.md` (Claude Code) and `~/.codex/AGENTS.md` (Codex).
//!
//! This is the discovery half of the session CLI: the commands exist either
//! way, but an agent only reaches for a tool it has been told about, and the
//! one place every agent reliably reads is its own instruction file. The note
//! is delimited by marker comments so install is idempotent (re-install
//! replaces the block in place — how the note gets updated across tty7
//! versions) and uninstall removes exactly what tty7 wrote, never the user's
//! own text around it.
//!
//! Consent lives elsewhere: the one-time "Let your agents coordinate?" prompt
//! (`ui::windows`) and the Settings → Agents toggle both call in here.

use std::path::PathBuf;

/// Block delimiters. The trailing text on the begin marker tells a user who
/// finds this in their file what it is and where the off switch lives.
const BEGIN: &str = "<!-- tty7:session-cli:begin — managed by tty7 (Settings → Agents) -->";
const END: &str = "<!-- tty7:session-cli:end -->";

/// The note itself. Terse on purpose: this rides in every session's context
/// window, so each line has to earn its tokens.
const NOTE: &str = "\
## tty7 session CLI (agent coordination)

Inside tty7 (the `TTY7` env var is set), the `tty7` CLI manages this
machine's terminal sessions — including panes running other coding agents.
`%N` is a pane id from `ls`; `$TTY7_PANE` is your own. Every verb takes
`--json`.

- `tty7 ls` — workspaces, tabs and panes at a glance
- `tty7 agents` — every agent and its status (idle/working/waiting/done)
- `tty7 tab new --cwd DIR` — a fresh shell pane, prints its id
- `tty7 send %N 'text' --enter` — type into a pane
- `tty7 wait %N --until waiting,done --timeout 600` — block until that pane's agent needs input or finishes (exit 124 on timeout)
- `tty7 capture %N --plain` — a pane's output as plain text

Delegate work by opening a pane, sending the peer agent a prompt, `wait`ing
on it, then capturing its output.";

/// Where the note goes: Claude Code's global instruction file always (it is
/// the flagship integration, and the directory is created if missing), plus
/// Codex's — but only when `~/.codex` already exists, so enabling this never
/// seeds config clutter for an agent the user doesn't run.
fn targets() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(dir) = claude_config_dir() {
        out.push(dir.join("CLAUDE.md"));
    }
    if let Some(home) = home_dir() {
        let codex = home.join(".codex");
        if codex.is_dir() {
            out.push(codex.join("AGENTS.md"));
        }
    }
    out
}

/// Claude Code's config dir, honoring the same `CLAUDE_CONFIG_DIR` override
/// its hooks installer honors.
fn claude_config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    Some(home_dir()?.join(".claude"))
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

/// Install (or refresh) the note in every target file. Returns a short
/// human-readable summary for the settings row.
pub fn install() -> anyhow::Result<String> {
    let targets = targets();
    if targets.is_empty() {
        anyhow::bail!("cannot resolve home directory");
    }
    for path in &targets {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let current = std::fs::read_to_string(path).unwrap_or_default();
        let updated = upsert_block(&current);
        crate::core::config::write_atomic(path, updated.as_bytes())?;
    }
    Ok("Enabled".to_string())
}

/// Remove the note from every file that carries it. User text is untouched —
/// only the marked block goes.
pub fn uninstall() -> anyhow::Result<String> {
    for path in targets() {
        let Ok(current) = std::fs::read_to_string(&path) else {
            continue;
        };
        let stripped = remove_block(&current);
        if stripped != current {
            crate::core::config::write_atomic(&path, stripped.as_bytes())?;
        }
    }
    Ok("Disabled".to_string())
}

/// Whether any target file currently carries the note.
pub fn installed() -> bool {
    targets().iter().any(|p| {
        std::fs::read_to_string(p)
            .map(|s| s.contains(BEGIN))
            .unwrap_or(false)
    })
}

/// Replace an existing block or append a fresh one — the single write shape
/// both install and version-refresh use.
fn upsert_block(content: &str) -> String {
    let mut out = remove_block(content);
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(BEGIN);
    out.push('\n');
    out.push_str(NOTE);
    out.push('\n');
    out.push_str(END);
    out.push('\n');
    out
}

/// Strip the marked block (and the blank line install added before it),
/// leaving everything else byte-identical. An unterminated block — the file
/// was hand-edited past recognition — is left alone rather than truncated at
/// a guess.
fn remove_block(content: &str) -> String {
    let Some(start) = content.find(BEGIN) else {
        return content.to_string();
    };
    let Some(end_at) = content[start..].find(END) else {
        return content.to_string();
    };
    let mut end = start + end_at + END.len();
    if content[end..].starts_with('\n') {
        end += 1;
    }
    let mut head = &content[..start];
    // Also swallow the separating blank line install inserted, so repeated
    // enable/disable can't grow a ladder of empty lines.
    while head.ends_with("\n\n") {
        head = &head[..head.len() - 1];
    }
    format!("{head}{}", &content[end..])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Install into an empty file, a file with user content, and over a stale
    /// copy of itself — the block must appear exactly once, after user text.
    #[test]
    fn upsert_is_idempotent_and_preserves_user_text() {
        let fresh = upsert_block("");
        assert!(fresh.starts_with(BEGIN));
        assert_eq!(fresh.matches(BEGIN).count(), 1);

        let with_user = upsert_block("# my rules\nalways be kind\n");
        assert!(with_user.starts_with("# my rules\nalways be kind\n"));
        assert_eq!(with_user.matches(BEGIN).count(), 1);

        // Re-upserting over an installed file must not duplicate the block —
        // this is also the version-refresh path.
        let again = upsert_block(&with_user);
        assert_eq!(again, with_user);
    }

    /// Remove restores the user's file byte-identically, however many
    /// enable/disable round trips happened.
    #[test]
    fn remove_restores_the_original() {
        let user = "# my rules\nalways be kind\n";
        let installed = upsert_block(user);
        assert_eq!(remove_block(&installed), user);
        // Round-trip stability: enable → disable → enable → disable.
        let twice = remove_block(&upsert_block(&remove_block(&installed)));
        assert_eq!(twice, user);
        // Removing from a file without the block is the identity.
        assert_eq!(remove_block(user), user);
    }

    /// A block whose end marker was lost to hand-editing is left untouched —
    /// never truncate someone's instruction file at a guess.
    #[test]
    fn unterminated_block_is_left_alone() {
        let mangled = format!("keep me\n{BEGIN}\nhalf a block…");
        assert_eq!(remove_block(&mangled), mangled);
    }
}

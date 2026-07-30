//! Shell discovery: enumerate the shells installed on this machine so the UI
//! can offer them in the new-tab dropdown, and resolve the platform default.
//!
//! Rather than asking the user to type a program path into config, probe the
//! well-known install locations up front and present what actually exists.
//!
//! - **Unix**: `/etc/shells` is the system's own inventory — parse it, keep the
//!   entries that exist, dedupe by basename (the same shell often appears as
//!   both `/bin/zsh` and `/usr/local/bin/zsh`). The login shell (`$SHELL`) is
//!   seeded first so it wins its dedupe slot and leads the list. Package
//!   managers don't register what they install there (Homebrew only *suggests*
//!   adding fish to `/etc/shells`), so a curated set of well-known shells is
//!   then probed on `PATH` as the catch-all.
//! - **Windows**: there is no inventory file, so probe each shell's known
//!   homes: PowerShell 7 across its six-ish install roots, Windows PowerShell
//!   in System32, cmd via `%ComSpec%`, Git Bash under the Git install, and WSL
//!   distributions via `wsl.exe -l -q`.
//!
//! Everything effectful (filesystem, env, spawning `wsl.exe`) stays in thin
//! wrappers; the parsing/selection logic is pure functions with unit tests.
//! Discovery can take a beat (WSL enumeration spawns a process), so callers
//! run [`detect_shells`] off the UI thread.

use std::path::Path;
// The probe helpers below build candidate paths; they're Windows-only code.
#[cfg(windows)]
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// One launchable shell surfaced in the new-tab dropdown. `program` + `args`
/// have the same shape as `config::ShellConfig` / `protocol::ShellSpec`: a
/// bare name resolved via `PATH` or an absolute path, plus launch arguments.
///
/// Serializable because the dropdown of a **remote** workspace's window lists
/// the shells of the machine that workspace lives on, not this one's: the list
/// crosses the control dialect as [`ShellInventory`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectedShell {
    /// Human-readable menu label, e.g. `zsh`, `PowerShell 7`, `WSL · Ubuntu`.
    pub label: String,
    pub program: String,
    pub args: Vec<String>,
}

impl DetectedShell {
    fn bare(label: impl Into<String>, program: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            program: program.into(),
            args: Vec::new(),
        }
    }
}

/// What one machine can launch: its shells, plus which of them a plain new tab
/// lands on. The unit the new-tab dropdown is built from.
///
/// Both halves have to come from the *same* machine. A remote workspace's
/// window that listed this computer's shells would offer `/bin/zsh` on a box
/// whose zsh is at `/usr/bin/zsh` — a picker whose every entry fails to spawn.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellInventory {
    pub shells: Vec<DetectedShell>,
    /// Short name of the shell a *default* spawn resolves to (`zsh`,
    /// `PowerShell 7`), for the menu's `default` tag.
    pub default_name: String,
}

/// This machine's [`ShellInventory`], honoring the `shell` override in the
/// config file *this process* reads.
///
/// The config lookup goes through [`crate::core::config::shell_command`] rather
/// than a GPUI global on purpose: the remote `tty7-server` answers this on the
/// far side of an SSH connection with no GUI in the process, and the override
/// that matters there is the one in *its* `config.json`.
///
/// Runs filesystem probes — call off the UI thread.
pub fn inventory() -> ShellInventory {
    let configured = crate::core::config::shell_command();
    ShellInventory {
        shells: detect_shells(),
        default_name: default_shell_name(configured.as_ref().map(|(p, _)| p.as_str())),
    }
}

/// Enumerate the shells installed on this machine, best-effort. Order is
/// meaningful: the entry most likely to be the user's default comes first.
/// Runs filesystem probes (and `wsl.exe` on Windows) — call off the UI thread.
pub fn detect_shells() -> Vec<DetectedShell> {
    #[cfg(unix)]
    {
        detect_unix()
    }
    #[cfg(windows)]
    {
        detect_windows()
    }
}

/// The short display name of the shell a *default* spawn resolves to: the
/// config override when set, otherwise the platform default (`$SHELL` on Unix,
/// the probed PowerShell on Windows). Drives the "Default (zsh)" menu label.
pub fn default_shell_name(configured: Option<&str>) -> String {
    let program = match configured {
        Some(p) if !p.trim().is_empty() => p.to_string(),
        _ => {
            #[cfg(unix)]
            {
                std::env::var("SHELL").unwrap_or_else(|_| "sh".into())
            }
            #[cfg(windows)]
            {
                windows_default_shell().to_string()
            }
        }
    };
    basename(&program)
}

/// The last path component of `program`, lowercased on Windows and stripped of
/// a trailing `.exe` — `C:\...\pwsh.exe` and `/usr/local/bin/fish` both reduce
/// to their bare shell name for labels and dedupe keys.
fn basename(program: &str) -> String {
    let base = Path::new(program)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| program.to_string());
    if cfg!(windows) {
        let lower = base.to_ascii_lowercase();
        lower.strip_suffix(".exe").unwrap_or(&lower).to_string()
    } else {
        base
    }
}

// ---------------------------------------------------------------------------
// Unix
// ---------------------------------------------------------------------------

/// Parse `/etc/shells` content: one absolute path per line, `#` comments and
/// blank lines skipped. Pure — the caller supplies the file content.
#[cfg_attr(windows, allow(dead_code))]
fn parse_etc_shells(content: &str) -> Vec<String> {
    content
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect()
}

/// Order + dedupe the Unix candidate list: keep the first occurrence of each
/// basename that `exists` confirms, labelled by that basename. Pure — `exists`
/// is injected so tests need no real filesystem.
#[cfg_attr(windows, allow(dead_code))]
fn unix_shells_from(
    candidates: impl IntoIterator<Item = String>,
    exists: impl Fn(&str) -> bool,
) -> Vec<DetectedShell> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for path in candidates {
        if !exists(&path) {
            continue;
        }
        let name = basename(&path);
        if seen.insert(name.clone()) {
            out.push(DetectedShell::bare(name, path));
        }
    }
    out
}

/// Shells package managers commonly install *without* registering them in
/// `/etc/shells` — Homebrew and nix leave that edit to the user, and few make
/// it, so `/etc/shells` misses e.g. a brew-installed fish entirely. Probed on
/// `PATH` (the login-shell-enriched one — see `enrich_path_from_login_shell`
/// in `main` — so Dock launches see Homebrew's prefix too).
#[cfg_attr(windows, allow(dead_code))]
const PATH_PROBED_SHELLS: [&str; 5] = ["fish", "nu", "pwsh", "elvish", "xonsh"];

/// Expand [`PATH_PROBED_SHELLS`] into concrete candidate paths, one per
/// `path_var` directory in `PATH` order. Fed through the same exists + dedupe
/// pass as the `/etc/shells` entries, so the first directory that actually
/// holds the shell wins — `which` semantics without spawning anything.
/// Relative `PATH` entries are skipped: a `./fish` candidate would resolve
/// somewhere else at every spawn. Pure — the caller supplies `path_var`.
#[cfg_attr(windows, allow(dead_code))]
fn path_shell_candidates(path_var: &str) -> Vec<String> {
    let dirs: Vec<&str> = path_var.split(':').filter(|d| d.starts_with('/')).collect();
    PATH_PROBED_SHELLS
        .iter()
        .flat_map(|name| {
            dirs.iter()
                .map(move |dir| format!("{}/{name}", dir.trim_end_matches('/')))
        })
        .collect()
}

#[cfg(unix)]
fn detect_unix() -> Vec<DetectedShell> {
    // Seed the login shell first so it wins its basename's dedupe slot and
    // leads the list — it also covers shells installed outside /etc/shells
    // (nix/homebrew installs the user pointed $SHELL at without registering).
    // The PATH probe comes last: registered shells keep their `/etc/shells`
    // paths, and only the unregistered leftovers (brew fish, nushell, …) are
    // picked up from `PATH`.
    let login = std::env::var("SHELL").ok().filter(|s| !s.is_empty());
    let etc = std::fs::read_to_string("/etc/shells").unwrap_or_default();
    let path_var = std::env::var("PATH").unwrap_or_default();
    let candidates = login
        .into_iter()
        .chain(parse_etc_shells(&etc))
        .chain(path_shell_candidates(&path_var));
    unix_shells_from(candidates, |p| Path::new(p).is_file())
}

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------

/// The Windows shell a *default* spawn launches: PowerShell 7 (`pwsh.exe`)
/// when installed, else Windows PowerShell. Probed once and cached — the
/// daemon consults this on every pane spawn.
#[cfg(windows)]
pub fn windows_default_shell() -> &'static str {
    use std::sync::OnceLock;
    static DEFAULT: OnceLock<String> = OnceLock::new();
    DEFAULT.get_or_init(|| {
        find_pwsh7()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "powershell.exe".to_string())
    })
}

/// Locate PowerShell 7: fixed install roots first (Program
/// Files x64/x86/ARM, dotnet tools, scoop, the Microsoft Store shim), then a
/// `PATH` search as the catch-all.
#[cfg(windows)]
fn find_pwsh7() -> Option<PathBuf> {
    let mut roots = Vec::new();
    for var in ["ProgramFiles", "ProgramFiles(x86)", "ProgramFiles(Arm)"] {
        if let Some(pf) = std::env::var_os(var).filter(|v| !v.is_empty()) {
            let pf = PathBuf::from(pf);
            roots.push(pf.join("PowerShell").join("7"));
            roots.push(pf.join("PowerShell").join("7-preview"));
        }
    }
    if let Some(home) = std::env::var_os("USERPROFILE").filter(|v| !v.is_empty()) {
        let home = PathBuf::from(home);
        roots.push(home.join(".dotnet").join("tools"));
        roots.push(home.join("scoop").join("shims"));
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty()) {
        roots.push(PathBuf::from(local).join("Microsoft").join("WindowsApps"));
    }
    pick_first_existing(roots.iter().map(|r| r.join("pwsh.exe")))
        .or_else(|| find_in_path("pwsh.exe"))
}

/// First candidate that exists on disk. Shared by the per-shell probes.
#[cfg(windows)]
fn pick_first_existing(candidates: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    candidates.into_iter().find(|p| p.is_file())
}

/// Minimal `PATH` search (no PATHEXT expansion — callers pass the full
/// `foo.exe` name).
#[cfg(windows)]
fn find_in_path(exe: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(exe))
        .find(|p| p.is_file())
}

#[cfg(windows)]
fn detect_windows() -> Vec<DetectedShell> {
    let mut out = Vec::new();
    let system_root =
        PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into()));

    if let Some(pwsh) = find_pwsh7() {
        out.push(DetectedShell::bare(
            "PowerShell 7",
            pwsh.to_string_lossy().into_owned(),
        ));
    }

    let ps5 = system_root
        .join("System32")
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe");
    if ps5.is_file() {
        out.push(DetectedShell::bare(
            "Windows PowerShell",
            ps5.to_string_lossy().into_owned(),
        ));
    }

    let cmd = std::env::var_os("ComSpec")
        .map(PathBuf::from)
        .filter(|p| p.is_file())
        .unwrap_or_else(|| system_root.join("System32").join("cmd.exe"));
    if cmd.is_file() {
        out.push(DetectedShell::bare(
            "Command Prompt",
            cmd.to_string_lossy().into_owned(),
        ));
    }

    if let Some(bash) = find_git_bash() {
        out.push(DetectedShell {
            label: "Git Bash".into(),
            program: bash.to_string_lossy().into_owned(),
            // Interactive login shell — matches Git Bash's own launcher. These
            // are tty7's args, not the user's, so shell integration may replace
            // them with its own spelling of the same thing (see
            // `protocol::ShellSpec::args_are_tty7_defaults`); they stand as the
            // fallback for when integration doesn't apply or fails to set up.
            args: vec!["-i".into(), "-l".into()],
        });
    }

    for distro in list_wsl_distros().unwrap_or_default() {
        out.push(DetectedShell {
            label: format!("WSL · {distro}"),
            program: "wsl.exe".into(),
            // `--cd ~` lands in the distro's home rather than a translated
            // Windows path the inner shell can't do much with.
            args: vec!["--distribution".into(), distro, "--cd".into(), "~".into()],
        });
    }

    out
}

/// Git Bash's `bash.exe`, if Git for Windows is installed. Exposed only to
/// tests, so `daemon::shell_integration`'s live-PTY check can spawn the same
/// binary the dropdown does (and skip itself when there is none).
#[cfg(all(windows, test))]
pub fn git_bash_path() -> Option<PathBuf> {
    find_git_bash()
}

/// Installed WSL distribution names, empty when WSL is absent — and always
/// empty off Windows, so callers need no `cfg` of their own.
///
/// Two callers want the same list for different reasons. [`detect_shells`]
/// offers a distro as a **shell** to launch in a pane; the workspace switcher
/// offers it as a **machine** that can host a remote workspace
/// (`ui::remote_connect::available_hosts`). Same enumeration, so the two lists
/// can never disagree about which distros exist.
///
/// Spawns `wsl.exe` on Windows — the same rule as [`detect_shells`]: call it
/// off the UI thread.
pub fn wsl_distros() -> Vec<String> {
    wsl_distros_probed().unwrap_or_default()
}

/// [`wsl_distros`], keeping the difference between *nothing is installed* and
/// *the probe could not answer*.
///
/// `Some(vec![])` is an answer — WSL is present and has no distributions, or this
/// is not Windows at all, where there can never be one. `None` means the probe
/// itself failed, and a caller holding a previous list should keep it rather than
/// report that the user's distributions have gone away: `wsl.exe` refuses while a
/// `wsl --shutdown` is in flight, which is a routine thing to run and a terrible
/// reason to empty the machine picker.
pub fn wsl_distros_probed() -> Option<Vec<String>> {
    #[cfg(windows)]
    {
        list_wsl_distros()
    }
    #[cfg(not(windows))]
    {
        Some(Vec::new())
    }
}

/// Git Bash from the usual Git-for-Windows install roots (machine-wide x64,
/// x86, and the per-user installer's home).
#[cfg(windows)]
fn find_git_bash() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    for var in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(pf) = std::env::var_os(var).filter(|v| !v.is_empty()) {
            candidates.push(PathBuf::from(pf).join("Git").join("bin").join("bash.exe"));
        }
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty()) {
        candidates.push(
            PathBuf::from(local)
                .join("Programs")
                .join("Git")
                .join("bin")
                .join("bash.exe"),
        );
    }
    pick_first_existing(candidates)
}

/// Installed WSL distribution names via `wsl.exe -l -q`.
///
/// **`None` is "the probe could not answer", not "there are none"** — no
/// `wsl.exe`, or one that failed, which is what a distribution mid-`wsl
/// --shutdown` or a broken WSL install looks like. `Some(vec![])` is the
/// authoritative empty answer: WSL is there and nothing is registered.
/// [`hide_console`](crate::core::proc::hide_console) keeps the probe from
/// flashing a console window (we're a GUI process).
#[cfg(windows)]
fn list_wsl_distros() -> Option<Vec<String>> {
    let mut cmd = std::process::Command::new("wsl.exe");
    cmd.args(["-l", "-q"]);
    let output = crate::core::proc::hide_console(&mut cmd).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(parse_wsl_list(&output.stdout))
}

/// Decode `wsl.exe -l -q` output — UTF-16LE, one distro per line — skipping
/// blanks and Docker Desktop's internal distros. Pure for testability.
#[cfg_attr(unix, allow(dead_code))]
fn parse_wsl_list(bytes: &[u8]) -> Vec<String> {
    // UTF-16LE: pair up bytes, tolerate a stray trailing byte.
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let text = String::from_utf16_lossy(&units);
    text.lines()
        .map(|l| l.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}' || c == '\0'))
        .filter(|l| !l.is_empty() && !l.starts_with("docker-desktop"))
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_etc_shells_skips_comments_and_blanks() {
        let content = "# /etc/shells\n\n/bin/sh\n/bin/bash\n  /bin/zsh  \n# trailing\n";
        assert_eq!(
            parse_etc_shells(content),
            vec!["/bin/sh", "/bin/bash", "/bin/zsh"]
        );
    }

    #[test]
    fn unix_shells_dedupe_by_basename_keeping_first() {
        // The login shell (seeded first) claims "zsh"; the /etc/shells copy of
        // zsh under another prefix is dropped; missing files are dropped.
        let candidates = [
            "/opt/homebrew/bin/zsh",
            "/bin/zsh",
            "/bin/bash",
            "/usr/local/bin/fish",
        ]
        .map(String::from);
        let exists = |p: &str| p != "/usr/local/bin/fish";
        let got = unix_shells_from(candidates, exists);
        assert_eq!(
            got,
            vec![
                DetectedShell::bare("zsh", "/opt/homebrew/bin/zsh"),
                DetectedShell::bare("bash", "/bin/bash"),
            ]
        );
    }

    #[test]
    fn path_shell_candidates_expand_dirs_in_order_skipping_relative() {
        let cands = path_shell_candidates("/opt/homebrew/bin:relative:.:/usr/bin/:");
        // Per shell, one candidate per *absolute* PATH dir, in PATH order, with
        // any trailing slash on the dir normalized away.
        assert_eq!(cands[0], "/opt/homebrew/bin/fish");
        assert_eq!(cands[1], "/usr/bin/fish");
        assert!(cands.contains(&"/opt/homebrew/bin/nu".to_string()));
        assert!(cands.iter().all(|c| c.starts_with('/')));
        assert_eq!(cands.len(), PATH_PROBED_SHELLS.len() * 2);
    }

    #[test]
    fn unregistered_path_shells_are_detected_after_etc_shells() {
        // A brew-installed fish: absent from /etc/shells (and not the login
        // shell), present on PATH — must still make the list, after the
        // registered shells. zsh exists on PATH too but keeps its /etc/shells
        // slot via the basename dedupe.
        let etc = ["/bin/zsh".to_string(), "/bin/bash".to_string()];
        let candidates = etc
            .into_iter()
            .chain(path_shell_candidates("/opt/homebrew/bin:/usr/bin"));
        let exists = |p: &str| {
            matches!(
                p,
                "/bin/zsh" | "/bin/bash" | "/opt/homebrew/bin/fish" | "/usr/bin/zsh"
            )
        };
        let got = unix_shells_from(candidates, exists);
        assert_eq!(
            got,
            vec![
                DetectedShell::bare("zsh", "/bin/zsh"),
                DetectedShell::bare("bash", "/bin/bash"),
                DetectedShell::bare("fish", "/opt/homebrew/bin/fish"),
            ]
        );
    }

    #[test]
    fn parse_wsl_list_decodes_utf16le_and_filters() {
        // "Ubuntu\r\ndocker-desktop\r\ndocker-desktop-data\r\nDebian\r\n\r\n"
        let text = "Ubuntu\r\ndocker-desktop\r\ndocker-desktop-data\r\nDebian\r\n\r\n";
        let bytes: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(parse_wsl_list(&bytes), vec!["Ubuntu", "Debian"]);
    }

    #[test]
    fn parse_wsl_list_tolerates_bom_and_empty_input() {
        assert_eq!(parse_wsl_list(&[]), Vec::<String>::new());
        let text = "\u{feff}Arch\r\n";
        let bytes: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(parse_wsl_list(&bytes), vec!["Arch"]);
    }

    #[test]
    fn basename_reduces_paths_to_shell_names() {
        assert_eq!(basename("/usr/local/bin/fish"), "fish");
        assert_eq!(basename("zsh"), "zsh");
        #[cfg(windows)]
        {
            assert_eq!(basename(r"C:\Program Files\PowerShell\7\pwsh.exe"), "pwsh");
            assert_eq!(basename("CMD.EXE"), "cmd");
        }
    }

    #[test]
    fn default_shell_name_prefers_the_configured_program() {
        assert_eq!(default_shell_name(Some("/usr/bin/fish")), "fish");
        assert_eq!(default_shell_name(Some("pwsh")), "pwsh");
        // Blank config falls through to the platform default — just assert it
        // yields *something* non-empty without pinning this host's $SHELL.
        assert!(!default_shell_name(None).is_empty());
        assert!(!default_shell_name(Some("  ")).is_empty());
    }
}

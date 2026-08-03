use std::path::Path;
#[cfg(windows)]
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectedShell {
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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellInventory {
    pub shells: Vec<DetectedShell>,
    pub default_name: String,
}

pub fn inventory() -> ShellInventory {
    let configured = crate::core::config::shell_command();
    ShellInventory {
        shells: detect_shells(),
        default_name: default_shell_name(configured.as_ref().map(|(p, _)| p.as_str())),
    }
}

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

pub fn default_shell_name(configured: Option<&str>) -> String {
    let program = match configured {
        Some(p) if !p.trim().is_empty() => p.to_string(),
        _ => login_shell(),
    };
    basename(&program)
}

/// The user's login shell, straight from the passwd database.
///
/// `$SHELL` is a snapshot taken when the session logged in, so `chsh` does not
/// move it — a GUI launch inherits whatever was current at login and keeps
/// reporting it until the user logs out. passwd is the live value; `$SHELL` is
/// only the fallback for the rare setup where the lookup fails (a directory
/// service that is down, a uid with no passwd entry).
pub fn login_shell() -> String {
    #[cfg(unix)]
    {
        pick_login_shell(passwd_shell(), std::env::var("SHELL").ok())
    }
    #[cfg(windows)]
    {
        windows_default_shell().to_string()
    }
}

#[cfg_attr(windows, allow(dead_code))]
fn pick_login_shell(passwd: Option<String>, env: Option<String>) -> String {
    passwd
        .into_iter()
        .chain(env)
        .map(|s| s.trim().to_string())
        .find(|s| !s.is_empty())
        .unwrap_or_else(|| "sh".into())
}

/// `getpwuid_r` — the reentrant form, because `getpwuid` hands back a pointer
/// into a shared static that another thread's lookup can overwrite under us.
#[cfg(unix)]
fn passwd_shell() -> Option<String> {
    let mut buf_len = match unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) } {
        n if n > 0 => n as usize,
        _ => 1024,
    };
    loop {
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut buf = vec![0 as libc::c_char; buf_len];
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        let rc = unsafe {
            libc::getpwuid_r(
                libc::getuid(),
                &mut pwd,
                buf.as_mut_ptr(),
                buf.len(),
                &mut found,
            )
        };
        // ERANGE just means the buffer was too small; anything else is fatal.
        if rc == libc::ERANGE && buf_len < 64 * 1024 {
            buf_len *= 2;
            continue;
        }
        if rc != 0 || found.is_null() || pwd.pw_shell.is_null() {
            return None;
        }
        let shell = unsafe { std::ffi::CStr::from_ptr(pwd.pw_shell) }
            .to_str()
            .ok()?
            .to_string();
        return Some(shell);
    }
}

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

#[cfg_attr(windows, allow(dead_code))]
fn parse_etc_shells(content: &str) -> Vec<String> {
    content
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect()
}

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

/// Shell names worth probing along `$PATH`.
///
/// The POSIX-y ones are here as well as the newer shells: `/etc/shells` lists
/// only what the system ships, so on a box with a Homebrew `bash` the entry
/// that wins the name is `/bin/bash` — macOS's 3.2 from 2007, which is old
/// enough that bash-completion 2.x refuses to load. Probing `$PATH` first makes
/// the menu's "bash" the same binary typing `bash` would reach.
#[cfg_attr(windows, allow(dead_code))]
const PATH_PROBED_SHELLS: [&str; 12] = [
    "bash", "zsh", "fish", "nu", "pwsh", "elvish", "xonsh", "sh", "ksh", "dash", "tcsh", "csh",
];

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
    let etc = std::fs::read_to_string("/etc/shells").unwrap_or_default();
    let path_var = std::env::var("PATH").unwrap_or_default();
    // Order decides who wins a name, since dedupe keeps the first: the login
    // shell must be reachable, then whatever `$PATH` resolves each name to,
    // and `/etc/shells` last to catch shells installed outside `$PATH`.
    let candidates = std::iter::once(login_shell())
        .chain(path_shell_candidates(&path_var))
        .chain(parse_etc_shells(&etc));
    unix_shells_from(candidates, |p| Path::new(p).is_file())
}

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

#[cfg(windows)]
fn pick_first_existing(candidates: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    candidates.into_iter().find(|p| p.is_file())
}

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
            args: vec!["-i".into(), "-l".into()],
        });
    }

    for distro in list_wsl_distros().unwrap_or_default() {
        out.push(DetectedShell {
            label: format!("WSL · {distro}"),
            program: "wsl.exe".into(),
            args: vec!["--distribution".into(), distro, "--cd".into(), "~".into()],
        });
    }

    out
}

#[cfg(all(windows, test))]
pub fn git_bash_path() -> Option<PathBuf> {
    find_git_bash()
}

pub fn wsl_distros() -> Vec<String> {
    wsl_distros_probed().unwrap_or_default()
}

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

#[cfg(windows)]
const WSL_LIST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

#[cfg(windows)]
fn list_wsl_distros() -> Option<Vec<String>> {
    let mut cmd = std::process::Command::new("wsl.exe");
    cmd.args(["-l", "-q"]);
    let output = match crate::core::proc::output_within(
        crate::core::proc::hide_console(&mut cmd),
        WSL_LIST_TIMEOUT,
    ) {
        Ok(output) => output,
        Err(e) => {
            log::warn!("could not list the WSL distros: {e}");
            return None;
        }
    };
    if !output.status.success() {
        return None;
    }
    Some(parse_wsl_list(&output.stdout))
}

#[cfg_attr(unix, allow(dead_code))]
fn parse_wsl_list(bytes: &[u8]) -> Vec<String> {
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
        // Each name walks $PATH in order, so the earlier dir gets first refusal.
        let first = PATH_PROBED_SHELLS[0];
        assert_eq!(cands[0], format!("/opt/homebrew/bin/{first}"));
        assert_eq!(cands[1], format!("/usr/bin/{first}"));
        assert!(cands.contains(&"/opt/homebrew/bin/nu".to_string()));
        assert!(cands.iter().all(|c| c.starts_with('/')));
        assert_eq!(cands.len(), PATH_PROBED_SHELLS.len() * 2);
    }

    #[test]
    fn etc_shells_still_contributes_what_path_does_not_reach() {
        let etc = ["/bin/zsh".to_string(), "/opt/weird/ksh".to_string()];
        let candidates = path_shell_candidates("/opt/homebrew/bin:/usr/bin")
            .into_iter()
            .chain(etc);
        let exists = |p: &str| {
            matches!(
                p,
                "/bin/zsh" | "/usr/bin/zsh" | "/opt/homebrew/bin/fish" | "/opt/weird/ksh"
            )
        };
        let got = unix_shells_from(candidates, exists);
        assert_eq!(
            got,
            vec![
                // $PATH resolves zsh to /usr/bin/zsh, so /bin/zsh loses the name
                DetectedShell::bare("zsh", "/usr/bin/zsh"),
                DetectedShell::bare("fish", "/opt/homebrew/bin/fish"),
                // never on $PATH, so only /etc/shells knows about it
                DetectedShell::bare("ksh", "/opt/weird/ksh"),
            ]
        );
    }

    /// The bug: `/etc/shells` lists `/bin/bash` (macOS 3.2) before a Homebrew
    /// `bash`, so dedupe-by-name handed the menu entry to the 2007 build.
    #[test]
    fn a_path_shell_beats_the_same_name_in_etc_shells() {
        let etc = [
            "/bin/bash".to_string(),
            "/opt/homebrew/bin/bash".to_string(),
        ];
        let candidates = path_shell_candidates("/opt/homebrew/bin:/usr/bin")
            .into_iter()
            .chain(etc.iter().cloned());
        let exists = |p: &str| matches!(p, "/bin/bash" | "/opt/homebrew/bin/bash");
        let got = unix_shells_from(candidates, exists);
        assert_eq!(
            got,
            vec![DetectedShell::bare("bash", "/opt/homebrew/bin/bash")]
        );

        // …and with the old ordering the stale one would have won.
        let old_order = etc
            .into_iter()
            .chain(path_shell_candidates("/opt/homebrew/bin"));
        assert_eq!(
            unix_shells_from(old_order, exists),
            vec![DetectedShell::bare("bash", "/bin/bash")]
        );
    }

    #[test]
    fn the_login_shell_outranks_path_for_its_own_name() {
        // A login shell that $PATH would otherwise resolve elsewhere still has
        // to be the entry the menu offers.
        let candidates =
            std::iter::once("/opt/custom/bin/zsh".to_string()).chain(path_shell_candidates("/bin"));
        let exists = |p: &str| matches!(p, "/opt/custom/bin/zsh" | "/bin/zsh" | "/bin/bash");
        let got = unix_shells_from(candidates, exists);
        assert_eq!(got[0], DetectedShell::bare("zsh", "/opt/custom/bin/zsh"));
        assert!(!got.iter().any(|s| s.program == "/bin/zsh"));
    }

    /// passwd is the live value; `$SHELL` is a login-time snapshot that `chsh`
    /// cannot move, so it must never win.
    #[test]
    fn login_shell_prefers_passwd_over_a_stale_env() {
        assert_eq!(
            pick_login_shell(
                Some("/opt/homebrew/bin/bash".into()),
                Some("/bin/zsh".into())
            ),
            "/opt/homebrew/bin/bash"
        );
        // passwd unreadable, or the entry is blank — fall back to $SHELL
        assert_eq!(pick_login_shell(None, Some("/bin/zsh".into())), "/bin/zsh");
        assert_eq!(
            pick_login_shell(Some("  ".into()), Some("/bin/zsh".into())),
            "/bin/zsh"
        );
        // neither available
        assert_eq!(pick_login_shell(None, None), "sh");
        assert_eq!(
            pick_login_shell(Some(String::new()), Some(String::new())),
            "sh"
        );
    }

    #[cfg(unix)]
    #[test]
    fn passwd_shell_reads_this_users_entry() {
        // Every account running the suite has a shell in passwd; the point is
        // that the lookup works and returns an absolute path, not a specific one.
        let got = passwd_shell().expect("passwd lookup should succeed");
        assert!(
            got.starts_with('/'),
            "expected an absolute path, got {got:?}"
        );
    }

    #[test]
    fn parse_wsl_list_decodes_utf16le_and_filters() {
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
        assert!(!default_shell_name(None).is_empty());
        assert!(!default_shell_name(Some("  ")).is_empty());
    }
}

#[cfg(windows)]
use std::ffi::OsStr;
use std::path::Path;
#[cfg(windows)]
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectedShell {
    pub label: String,
    pub program: String,
    pub args: Vec<String>,
    /// Marks arguments supplied by tty7's shell detection rather than by the user.
    /// Older peers omit this field, and their inventory arguments were all treated
    /// as tty7 defaults, so `true` preserves the previous protocol behavior.
    #[serde(default = "default_true")]
    pub args_are_tty7_defaults: bool,
}

impl DetectedShell {
    fn bare(label: impl Into<String>, program: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            program: program.into(),
            args: Vec::new(),
            args_are_tty7_defaults: true,
        }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellInventory {
    pub shells: Vec<DetectedShell>,
    pub default_name: String,
}

pub fn inventory() -> ShellInventory {
    let configured = crate::core::config::shell_command();
    inventory_from(detect_shells(), configured, &login_shell())
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

/// Combines the detected shells with the explicit shell from Settings.
///
/// A bare configured command such as `nu` matches the detected executable with
/// the same basename because both resolve through PATH. Explicit paths only
/// match the same path, so choosing a second installation of the same shell
/// still creates a useful, distinct menu entry. The detected label wins for a
/// match, preserving friendly names such as "Nushell" and "PowerShell 7".
fn inventory_from(
    mut shells: Vec<DetectedShell>,
    configured: Option<(String, Vec<String>)>,
    fallback_program: &str,
) -> ShellInventory {
    let configured = configured.filter(|(program, _)| !program.trim().is_empty());
    let default_program = configured
        .as_ref()
        .map_or(fallback_program, |(program, _)| program.as_str());

    if let Some(default_index) = shells
        .iter()
        .position(|shell| same_shell_program(&shell.program, default_program))
    {
        // A configured command may resolve to an already detected executable.
        // Keep the detected friendly label, but retain the user's command,
        // launch arguments, and their origin so local and remote menus behave
        // identically.
        if let Some((program, args)) = configured.as_ref() {
            // Keep a bare command bare: it must continue to resolve through PATH.
            // The detected entry can be the login shell (which intentionally wins
            // inventory deduplication), and that absolute path is not necessarily
            // the executable the configured bare command would resolve to.
            shells[default_index].program.clone_from(program);
            shells[default_index].args.clone_from(args);
            shells[default_index].args_are_tty7_defaults = false;
        }
        let default_name = shells[default_index].label.clone();
        return ShellInventory {
            shells,
            default_name,
        };
    }

    let mut default_name = basename(default_program);
    if let Some((program, args)) = configured {
        if shells.iter().any(|shell| shell.label == default_name) {
            default_name.push_str(" (Configured)");
        }
        // The configured shell is the platform default, so keep it at the top
        // just like the detected login shell while retaining its custom args.
        shells.insert(
            0,
            DetectedShell {
                label: default_name.clone(),
                program,
                args,
                args_are_tty7_defaults: false,
            },
        );
    }

    ShellInventory {
        shells,
        default_name,
    }
}

/// Compares executable identities without collapsing two explicit installs.
fn same_shell_program(detected: &str, configured: &str) -> bool {
    let detected = detected.trim();
    let configured = configured.trim();
    if detected.is_empty() || configured.is_empty() {
        return false;
    }

    let detected_path = Path::new(detected);
    let configured_path = Path::new(configured);
    if is_bare_program(configured_path) {
        return basename(detected) == basename(configured);
    }
    if is_bare_program(detected_path) {
        return false;
    }

    comparable_program_path(detected_path) == comparable_program_path(configured_path)
}

fn is_bare_program(program: &Path) -> bool {
    program.components().count() <= 1
}

/// Canonicalization folds harmless `.` and `..` differences when the target
/// exists. The textual fallback still gives stable behavior for configured
/// paths that have not been installed yet.
fn comparable_program_path(program: &Path) -> String {
    let resolved = std::fs::canonicalize(program).unwrap_or_else(|_| program.to_path_buf());
    let mut value = resolved.to_string_lossy().into_owned();
    if cfg!(windows) {
        value = value.replace('/', "\\").to_ascii_lowercase();
    }
    value
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

/// Keeps conventional executable names for POSIX shells while giving Nushell
/// the same product name in the menu on every supported desktop platform.
fn shell_label(name: &str) -> String {
    if name.eq_ignore_ascii_case("nu") {
        "Nushell".to_string()
    } else {
        name.to_string()
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
            out.push(DetectedShell::bare(shell_label(&name), path));
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
    find_in_path_var(exe, &path)
}

/// Finds an exact executable name using Windows' ordered `PATH` directories.
///
/// The caller supplies the `.exe` suffix explicitly so a directory with the
/// same name is never mistaken for a launchable shell. Keeping the path value
/// separate also makes the lookup testable without mutating process-global
/// environment variables while other tests are running.
#[cfg(windows)]
fn find_in_path_var(exe: &str, path: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(path)
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

    // Nushell has no stable Windows installation directory across package
    // managers, so mirror command resolution and offer the first `nu.exe`
    // reachable through the current system PATH.
    if let Some(nu) = find_in_path("nu.exe") {
        out.push(DetectedShell::bare(
            "Nushell",
            nu.to_string_lossy().into_owned(),
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
            args_are_tty7_defaults: true,
        });
    }

    for distro in list_wsl_distros().unwrap_or_default() {
        out.push(DetectedShell {
            label: format!("WSL · {distro}"),
            program: "wsl.exe".into(),
            args: vec!["--distribution".into(), distro, "--cd".into(), "~".into()],
            args_are_tty7_defaults: true,
        });
    }

    out
}

#[cfg(all(windows, test))]
pub fn git_bash_path() -> Option<PathBuf> {
    find_git_bash()
}

#[cfg(all(windows, test))]
mod wsl_tests {
    #[test]
    fn the_default_distro_is_one_of_the_installed_ones() {
        let installed = super::wsl_distros();
        if installed.is_empty() {
            eprintln!("skipping: no WSL distributions installed");
            return;
        }
        let default = super::default_wsl_distro()
            .expect("a machine with installed distros names a default in Lxss");
        assert!(
            installed.contains(&default),
            "registry default {default:?} not in {installed:?}"
        );
    }
}

pub fn wsl_distros() -> Vec<String> {
    wsl_distros_probed().unwrap_or_default()
}

/// The distro `wsl.exe` launches when no `--distribution` is given, read from
/// the registry (`Lxss\DefaultDistribution` names the per-distro key that
/// carries `DistributionName`). The registry rather than `wsl -l`: this runs
/// on the pane-spawn path, where a microsecond read beats a subprocess.
#[cfg(windows)]
pub fn default_wsl_distro() -> Option<String> {
    const LXSS: &str = r"Software\Microsoft\Windows\CurrentVersion\Lxss";
    let guid = registry_user_string(LXSS, "DefaultDistribution")?;
    let name = registry_user_string(&format!(r"{LXSS}\{guid}"), "DistributionName")?;
    (!name.is_empty()).then_some(name)
}

#[cfg(not(windows))]
pub fn default_wsl_distro() -> Option<String> {
    None
}

#[cfg(windows)]
fn registry_user_string(subkey: &str, value: &str) -> Option<String> {
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_SZ, RegGetValueW};

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }
    let subkey = wide(subkey);
    let value = wide(value);
    let mut bytes: u32 = 0;
    // SAFETY: a null data pointer makes this a pure sizing call; the key and
    // value names are NUL-terminated UTF-16 owned right above.
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut bytes,
        )
    };
    if rc != 0 || bytes == 0 {
        return None;
    }
    let mut buf = vec![0u16; (bytes as usize).div_ceil(2)];
    let mut size = bytes;
    // SAFETY: `buf` is `size` bytes as the sizing call reported;
    // RegGetValueW writes at most that many and NUL-terminates REG_SZ data.
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if rc != 0 {
        return None;
    }
    let len = buf.iter().position(|&u| u == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]))
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
    fn nushell_is_probed_from_every_unix_path_directory() {
        let candidates = path_shell_candidates("/opt/homebrew/bin:/home/user/.local/bin");
        let nushell: Vec<_> = candidates
            .iter()
            .filter(|candidate| candidate.ends_with("/nu"))
            .map(String::as_str)
            .collect();

        assert_eq!(
            nushell,
            ["/opt/homebrew/bin/nu", "/home/user/.local/bin/nu"]
        );

        let detected = unix_shells_from(candidates, |candidate| {
            candidate == "/home/user/.local/bin/nu"
        });
        assert_eq!(
            detected,
            [DetectedShell::bare("Nushell", "/home/user/.local/bin/nu")]
        );
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

    #[cfg(windows)]
    #[test]
    fn windows_path_probe_finds_the_first_nushell_file() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let directory_named_like_nu = first.path().join("nu.exe");
        std::fs::create_dir(&directory_named_like_nu).unwrap();
        let expected = second.path().join("nu.exe");
        std::fs::write(&expected, b"test executable placeholder").unwrap();

        let path = std::env::join_paths([first.path(), second.path()]).unwrap();
        assert_eq!(find_in_path_var("nu.exe", &path), Some(expected));
    }

    #[test]
    fn a_unique_configured_shell_is_added_first_with_its_args() {
        let detected = vec![DetectedShell::bare("System Shell", "system-shell")];
        let inventory = inventory_from(
            detected,
            Some((
                "custom-shell".into(),
                vec!["--login".into(), "--verbose".into()],
            )),
            "system-shell",
        );

        assert_eq!(inventory.default_name, "custom-shell");
        assert_eq!(inventory.shells.len(), 2);
        assert_eq!(inventory.shells[0].label, "custom-shell");
        assert_eq!(inventory.shells[0].program, "custom-shell");
        assert_eq!(inventory.shells[0].args, ["--login", "--verbose"]);
        assert!(!inventory.shells[0].args_are_tty7_defaults);
    }

    #[test]
    fn a_bare_configured_name_reuses_the_detected_friendly_entry() {
        let detected_program = if cfg!(windows) {
            r"C:\Tools\Nushell\nu.exe"
        } else {
            "/opt/nushell/bin/nu"
        };
        let inventory = inventory_from(
            vec![DetectedShell::bare("Nushell", detected_program)],
            Some(("nu".into(), vec!["--login".into()])),
            "fallback-shell",
        );

        assert_eq!(inventory.shells.len(), 1, "the same shell was duplicated");
        assert_eq!(inventory.default_name, "Nushell");
        assert_eq!(inventory.shells[0].program, "nu");
        assert_eq!(inventory.shells[0].args, ["--login"]);
        assert!(!inventory.shells[0].args_are_tty7_defaults);
    }

    #[test]
    fn a_bare_configured_command_keeps_path_resolution_after_deduplication() {
        let inventory = inventory_from(
            vec![DetectedShell::bare("bash", "/bin/bash")],
            Some(("bash".into(), Vec::new())),
            "fallback-shell",
        );

        assert_eq!(inventory.shells.len(), 1);
        assert_eq!(inventory.shells[0].program, "bash");
        assert_eq!(inventory.default_name, "bash");
    }

    #[test]
    fn explicit_same_named_shells_at_different_paths_stay_distinct() {
        let first = if cfg!(windows) {
            r"C:\Shells\first\custom.exe"
        } else {
            "/opt/shells/first/custom"
        };
        let second = if cfg!(windows) {
            r"D:\Shells\second\custom.exe"
        } else {
            "/opt/shells/second/custom"
        };
        let inventory = inventory_from(
            vec![DetectedShell::bare("custom", first)],
            Some((second.into(), Vec::new())),
            "fallback-shell",
        );

        assert_eq!(inventory.shells.len(), 2);
        assert_eq!(inventory.shells[0].program, second);
        assert_eq!(inventory.shells[0].label, "custom (Configured)");
        assert_eq!(inventory.shells[1].label, "custom");
        assert_eq!(inventory.default_name, "custom (Configured)");
    }

    #[test]
    fn an_explicit_configured_path_does_not_collapse_a_bare_detected_name() {
        let configured = if cfg!(windows) {
            r"C:\Shells\custom.exe"
        } else {
            "/opt/shells/custom"
        };
        let detected = if cfg!(windows) {
            "custom.exe"
        } else {
            "custom"
        };
        let inventory = inventory_from(
            vec![DetectedShell::bare("custom", detected)],
            Some((configured.into(), Vec::new())),
            "fallback-shell",
        );

        assert_eq!(inventory.shells.len(), 2);
        assert_eq!(inventory.shells[0].program, configured);
    }

    #[test]
    fn detected_label_names_the_unconfigured_platform_default() {
        let program = if cfg!(windows) {
            r"C:\Program Files\PowerShell\7\pwsh.exe"
        } else {
            "/opt/homebrew/bin/zsh"
        };
        let label = if cfg!(windows) { "PowerShell 7" } else { "zsh" };
        let inventory = inventory_from(vec![DetectedShell::bare(label, program)], None, program);

        assert_eq!(inventory.default_name, label);
        assert_eq!(inventory.shells.len(), 1);
        assert!(inventory.shells[0].args_are_tty7_defaults);
    }

    #[test]
    fn inventories_from_older_peers_treat_arguments_as_tty7_defaults() {
        let inventory: ShellInventory = serde_json::from_str(
            r#"{"shells":[{"label":"Git Bash","program":"bash","args":["-l"]}],"default_name":"Git Bash"}"#,
        )
        .expect("an inventory without argument-origin metadata should remain compatible");

        assert!(inventory.shells[0].args_are_tty7_defaults);
    }

    #[test]
    fn default_shell_name_prefers_the_configured_program() {
        assert_eq!(default_shell_name(Some("/usr/bin/fish")), "fish");
        assert_eq!(default_shell_name(Some("pwsh")), "pwsh");
        assert!(!default_shell_name(None).is_empty());
        assert!(!default_shell_name(Some("  ")).is_empty());
    }
}

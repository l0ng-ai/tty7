use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::AsyncWriteExt as _;

use super::{
    ExecOutput, InstallError, Installer, LoadedBinary, RemoteOps, RemoteStat, ServerBinarySource,
    install_confirm, shell_quote,
};

pub const WSL_EXE: &str = "wsl.exe";

const MARKER: &str = "__tty7_wsl__";

const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(30);
const PUT_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DistroNameError {
    Empty,
    LeadingDash(String),
    Control(String),
    TooLong(usize),
}

impl std::fmt::Display for DistroNameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "a WSL distribution name cannot be empty"),
            Self::LeadingDash(name) => write!(
                f,
                "{name:?} cannot be a WSL distribution name: a leading `-` would be read as an option"
            ),
            Self::Control(name) => write!(
                f,
                "{name:?} cannot be a WSL distribution name: it contains a control character"
            ),
            Self::TooLong(len) => write!(f, "a WSL distribution name of {len} bytes is not real"),
        }
    }
}

impl std::error::Error for DistroNameError {}

impl From<DistroNameError> for io::Error {
    fn from(e: DistroNameError) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidInput, e.to_string())
    }
}

const MAX_DISTRO_NAME: usize = 255;

pub fn validate_distro(name: &str) -> Result<(), DistroNameError> {
    if name.is_empty() {
        return Err(DistroNameError::Empty);
    }
    if name.len() > MAX_DISTRO_NAME {
        return Err(DistroNameError::TooLong(name.len()));
    }
    if name.starts_with('-') {
        return Err(DistroNameError::LeadingDash(name.to_string()));
    }
    if name.chars().any(|c| c.is_control()) {
        return Err(DistroNameError::Control(name.to_string()));
    }
    Ok(())
}

pub fn wsl_args(distro: &str, argv: &[&str]) -> Vec<String> {
    let mut args = vec!["-d".to_string(), distro.to_string(), "--".to_string()];
    args.extend(argv.iter().map(|a| (*a).to_string()));
    args
}

pub fn host_label(distro: &str) -> String {
    format!("wsl:{distro}")
}

pub fn list_distros() -> Vec<String> {
    let mut cmd = std::process::Command::new(WSL_EXE);
    cmd.args(["-l", "-q"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let Ok(out) = crate::core::proc::hide_console(&mut cmd).output() else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    parse_distro_list(&out.stdout)
}

pub fn parse_distro_list(bytes: &[u8]) -> Vec<String> {
    decode_wsl_text(bytes)
        .lines()
        .map(|line| line.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}' || c == '\0'))
        .filter(|line| !line.is_empty() && !line.starts_with("docker-desktop"))
        .map(str::to_string)
        .collect()
}

pub fn decode_wsl_text(bytes: &[u8]) -> String {
    if !bytes.contains(&0) {
        return strip_bom(&String::from_utf8_lossy(bytes));
    }
    bytes
        .split(|b| *b == b'\n')
        .map(decode_wsl_line)
        .collect::<Vec<_>>()
        .join("\n")
}

fn decode_wsl_line(line: &[u8]) -> String {
    let line = line.strip_prefix(&[0]).unwrap_or(line);
    if !line.contains(&0) {
        return strip_bom(&String::from_utf8_lossy(line));
    }
    let units: Vec<u16> = line
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    strip_bom(&String::from_utf16_lossy(&units))
}

fn strip_bom(s: &str) -> String {
    s.trim_start_matches('\u{feff}').to_string()
}

const HOME_SCRIPT: &str = "printf '__tty7_wsl__ home=%s\\n' \"$HOME\"\n";

const CD_HOME: &str = "cd \"$HOME\" 2>/dev/null || cd /\n";

const SETTLE_TRIES: u32 = 25;
const SETTLE_STEP: &str = "0.2";
const SETTLE_GRACE: &str = "0.3";

pub(super) fn launch_settle(binary: &str) -> String {
    settle_script(binary, SETTLE_TRIES, SETTLE_STEP, SETTLE_GRACE)
}

fn settle_script(binary: &str, tries: u32, step: &str, grace: &str) -> String {
    let bin = shell_quote(binary);
    format!(
        "__tty7_settle=0\n\
         while [ \"$__tty7_settle\" -lt {tries} ]; do\n\
         if {bin} --stdio --bridge < /dev/null > /dev/null 2>&1; then break; fi\n\
         __tty7_settle=$((__tty7_settle + 1))\n\
         sleep {step}\n\
         done\n\
         sleep {grace}\n"
    )
}

const _: () = assert!(
    konst_contains(HOME_SCRIPT, MARKER),
    "HOME_SCRIPT must carry MARKER, or `home_dir` silently stops parsing"
);

const fn konst_contains(haystack: &str, needle: &str) -> bool {
    let (h, n) = (haystack.as_bytes(), needle.as_bytes());
    if n.len() > h.len() {
        return false;
    }
    let mut start = 0;
    while start + n.len() <= h.len() {
        let mut i = 0;
        while i < n.len() && h[start + i] == n[i] {
            i += 1;
        }
        if i == n.len() {
            return true;
        }
        start += 1;
    }
    false
}

fn stat_script(path: &str) -> String {
    let p = shell_quote(path);
    format!(
        "p={p}\n\
         if [ -d \"$p\" ]; then printf '{MARKER} stat=dir 0 0\\n'\n\
         elif [ -e \"$p\" ]; then\n\
         if [ -x \"$p\" ]; then m=755; else m=644; fi\n\
         s=$(wc -c < \"$p\" 2>/dev/null) || s=0\n\
         printf '{MARKER} stat=file %s %s\\n' \"$m\" \"$s\"\n\
         else printf '{MARKER} stat=none 0 0\\n'\n\
         fi\n"
    )
}

fn parse_stat(stdout: &str) -> Result<Option<RemoteStat>, String> {
    for line in stdout.lines() {
        let Some(rest) = marked(line, "stat=") else {
            continue;
        };
        let mut words = rest.split_whitespace();
        let kind = words.next().unwrap_or_default();
        let mode = words
            .next()
            .and_then(|m| u32::from_str_radix(m, 8).ok())
            .unwrap_or(0);
        let size = words
            .next()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        return Ok(match kind {
            "none" => None,
            "dir" => Some(RemoteStat {
                size: 0,
                mode: 0o755,
                is_dir: true,
            }),
            "file" => Some(RemoteStat {
                size,
                mode,
                is_dir: false,
            }),
            other => return Err(format!("the distribution answered stat={other:?}")),
        });
    }
    Err(format!(
        "the distribution did not answer the stat probe (got {:?})",
        truncate(stdout)
    ))
}

fn list_script(dir: &str) -> String {
    let d = shell_quote(dir);
    format!(
        "d={d}\n\
         if [ -d \"$d\" ]; then\n\
         printf '{MARKER} listing\\n'\n\
         ls -A -- \"$d\" 2>/dev/null | while IFS= read -r e; do printf '{MARKER} entry=%s\\n' \"$e\"; done\n\
         else printf '{MARKER} nolisting\\n'\n\
         fi\n"
    )
}

fn parse_list(stdout: &str) -> Result<Option<Vec<String>>, String> {
    let mut saw_dir = false;
    let mut entries = Vec::new();
    for line in stdout.lines() {
        let Some(rest) = marked(line, "") else {
            continue;
        };
        if rest == "nolisting" {
            return Ok(None);
        }
        if rest == "listing" {
            saw_dir = true;
        } else if let Some(name) = rest.strip_prefix("entry=") {
            entries.push(name.to_string());
        }
    }
    if saw_dir {
        Ok(Some(entries))
    } else {
        Err(format!(
            "the distribution did not answer the directory probe (got {:?})",
            truncate(stdout)
        ))
    }
}

fn marked<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.trim()
        .strip_prefix(MARKER)?
        .trim_start()
        .strip_prefix(key)
}

fn truncate(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() <= 200 {
        return s.to_string();
    }
    s.chars().take(200).collect::<String>() + "…"
}

pub struct WslRemoteOps {
    distro: String,
}

impl WslRemoteOps {
    pub fn new(distro: impl Into<String>) -> Self {
        Self {
            distro: distro.into(),
        }
    }

    fn sh(&self, script: &str, timeout: Duration) -> Result<ExecOutput, String> {
        let script = format!("{CD_HOME}{script}");
        self.invoke(&["sh", "-s"], Some(script.into_bytes()), false, timeout)
    }

    fn sh_ok(&self, script: &str) -> Result<(), String> {
        let out = self.sh(script, COMMAND_TIMEOUT)?;
        if out.success() {
            Ok(())
        } else {
            Err(out.failure_reason())
        }
    }

    fn invoke(
        &self,
        argv: &[&str],
        input: Option<Vec<u8>>,
        discard_stdout: bool,
        timeout: Duration,
    ) -> Result<ExecOutput, String> {
        let args = wsl_args(&self.distro, argv);
        crate::daemon::ssh::SshManager::global()
            .handle()
            .block_on(async move { invoke_async(&args, input, discard_stdout, timeout).await })
    }
}

async fn invoke_async(
    args: &[String],
    input: Option<Vec<u8>>,
    discard_stdout: bool,
    timeout: Duration,
) -> Result<ExecOutput, String> {
    let mut cmd = tokio::process::Command::new(WSL_EXE);
    cmd.args(args)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(if discard_stdout {
            Stdio::null()
        } else {
            Stdio::piped()
        })
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    crate::core::proc::hide_console_tokio(&mut cmd);

    let mut child = cmd.spawn().map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => {
            format!("{WSL_EXE} is not available on this machine, so WSL cannot be used")
        }
        _ => format!("could not start {WSL_EXE}: {e}"),
    })?;

    let stdin = child.stdin.take();
    let feed = async move {
        if let (Some(mut stdin), Some(bytes)) = (stdin, input) {
            stdin.write_all(&bytes).await?;
            stdin.flush().await?;
            stdin.shutdown().await?;
        }
        Ok::<(), io::Error>(())
    };

    let both = async { tokio::join!(feed, child.wait_with_output()) };
    let (written, waited) = tokio::time::timeout(timeout, both)
        .await
        .map_err(|_| format!("the distribution did not answer within {timeout:?}"))?;

    let write_err = written.err();
    let out = waited.map_err(|e| format!("{WSL_EXE} failed: {e}"))?;

    let stdout = decode_wsl_text(&out.stdout);
    let stderr = decode_wsl_text(&out.stderr);
    if let Some(e) = write_err
        && out.status.success()
    {
        return Err(format!("could not send input to the distribution: {e}"));
    }

    Ok(ExecOutput {
        status: out.status.code().map(|c| c as u32),
        stdout,
        stderr,
    })
}

impl RemoteOps for WslRemoteOps {
    fn home_dir(&self) -> Result<String, String> {
        let out = self.sh(HOME_SCRIPT, COMMAND_TIMEOUT)?;
        if !out.success() {
            return Err(out.failure_reason());
        }
        match out.stdout.lines().find_map(|l| marked(l, "home=")) {
            Some(home) if home.starts_with('/') => Ok(home.to_string()),
            Some(home) => Err(format!(
                "the distribution resolved its home to {home:?}, which is not absolute"
            )),
            None => Err(format!(
                "the distribution did not report a home directory (got {:?})",
                truncate(&out.stdout)
            )),
        }
    }

    fn run(&self, cmd: &str) -> Result<ExecOutput, String> {
        self.sh(cmd, COMMAND_TIMEOUT)
    }

    fn spawn_detached(&self, cmd: &str) -> Result<(), String> {
        self.sh(cmd, LAUNCH_TIMEOUT).map(|_| ())
    }

    fn launch_settle(&self, binary: &str) -> Option<String> {
        Some(launch_settle(binary))
    }

    fn stat(&self, path: &str) -> Result<Option<RemoteStat>, String> {
        let out = self.sh(&stat_script(path), COMMAND_TIMEOUT)?;
        if !out.success() {
            return Err(out.failure_reason());
        }
        parse_stat(&out.stdout)
    }

    fn mkdir(&self, path: &str) -> Result<(), String> {
        self.sh_ok(&format!("mkdir -p {}\n", shell_quote(path)))
    }

    fn chmod(&self, path: &str, mode: u32) -> Result<(), String> {
        self.sh_ok(&format!("chmod {mode:o} {}\n", shell_quote(path)))
    }

    fn put(&self, path: &str, bytes: &[u8]) -> Result<(), String> {
        let out = self.invoke(&["tee", path], Some(bytes.to_vec()), true, PUT_TIMEOUT)?;
        if !out.success() {
            return Err(out.failure_reason());
        }
        match self.stat(path)? {
            Some(stat) if stat.size == bytes.len() as u64 => Ok(()),
            Some(stat) => Err(format!(
                "wrote {} bytes but the distribution reports {} at that path",
                bytes.len(),
                stat.size
            )),
            None => Err("the file was not there after writing it".to_string()),
        }
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), String> {
        self.sh_ok(&format!(
            "mv -f {} {}\n",
            shell_quote(from),
            shell_quote(to)
        ))
    }

    fn remove_file(&self, path: &str) -> Result<(), String> {
        self.sh_ok(&format!("rm -f {}\n", shell_quote(path)))
    }

    fn list_dir(&self, path: &str) -> Result<Option<Vec<String>>, String> {
        let out = self.sh(&list_script(path), COMMAND_TIMEOUT)?;
        if !out.success() {
            return Err(out.failure_reason());
        }
        parse_list(&out.stdout)
    }
}

pub const BUNDLED_DIR_ENV: &str = "TTY7_BUNDLED_SERVER_DIR";

pub const BUNDLED_SUBDIR: &str = "server";

pub fn bundled_search_dirs(exe: Option<&Path>, override_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut push = |d: PathBuf| {
        if !dirs.contains(&d) {
            dirs.push(d);
        }
    };
    if let Some(dir) = override_dir {
        push(dir.to_path_buf());
    }
    if let Some(exe_dir) = exe.and_then(Path::parent) {
        push(exe_dir.join(BUNDLED_SUBDIR));
        push(exe_dir.to_path_buf());
        if let Some(parent) = exe_dir.parent() {
            push(parent.join("Resources").join(BUNDLED_SUBDIR));
        }
    }
    dirs
}

pub struct BundledServerBinary {
    dirs: Vec<PathBuf>,
}

impl BundledServerBinary {
    pub fn discover() -> Self {
        let exe = std::env::current_exe().ok();
        let over = std::env::var_os(BUNDLED_DIR_ENV)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from);
        Self::in_dirs(bundled_search_dirs(exe.as_deref(), over.as_deref()))
    }

    pub fn in_dirs(dirs: Vec<PathBuf>) -> Self {
        Self { dirs }
    }

    pub fn from_env_only() -> Option<Self> {
        let dir = std::env::var_os(BUNDLED_DIR_ENV).filter(|v| !v.is_empty())?;
        Some(Self::in_dirs(vec![PathBuf::from(dir)]))
    }

    pub fn locate(&self, asset: &str) -> Option<PathBuf> {
        self.dirs
            .iter()
            .map(|dir| dir.join(asset))
            .find(|path| path.is_file())
    }
}

impl ServerBinarySource for BundledServerBinary {
    fn load(&self, _version: &str, asset: &'static str) -> Result<LoadedBinary, InstallError> {
        let missing = |extra: Option<String>| InstallError::MissingBundled {
            asset,
            searched: match extra {
                Some(one) => vec![one],
                None => self.dirs.iter().map(|d| d.display().to_string()).collect(),
            },
        };
        let Some(path) = self.locate(asset) else {
            return Err(missing(None));
        };
        let bytes =
            std::fs::read(&path).map_err(|e| missing(Some(format!("{} ({e})", path.display()))))?;
        if bytes.is_empty() {
            return Err(missing(Some(format!("{} (empty file)", path.display()))));
        }
        Ok(LoadedBinary {
            bytes,
            origin: path.display().to_string(),
        })
    }
}

static INSTALL_LOCKS: Mutex<Vec<(String, Arc<Mutex<()>>)>> = Mutex::new(Vec::new());

fn install_lock(distro: &str) -> Arc<Mutex<()>> {
    let mut locks = INSTALL_LOCKS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((_, lock)) = locks.iter().find(|(d, _)| d == distro) {
        return lock.clone();
    }
    let lock = Arc::new(Mutex::new(()));
    locks.push((distro.to_string(), lock.clone()));
    lock
}

pub fn ensure_wsl_server(distro: &str) -> io::Result<String> {
    validate_distro(distro)?;
    let lock = install_lock(distro);
    let _held = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

    let ops = WslRemoteOps::new(distro);
    let source = BundledServerBinary::discover();
    let confirm = install_confirm();
    let host = host_label(distro);
    let report = Installer::with_source(&ops, &source, confirm.as_ref(), host.clone()).run()?;

    log::info!(
        "{host}: {} at {} ({}{})",
        if report.installed {
            "installed the bundled tty7-server"
        } else {
            "tty7-server already present"
        },
        report.paths.binary,
        if report.launched {
            "daemon launched"
        } else {
            "daemon already running"
        },
        if report.mismatch.is_some() {
            ", build mismatch recorded"
        } else {
            ""
        },
    );
    Ok(report.paths.binary)
}

pub fn restart_wsl_daemon(distro: &str) -> io::Result<()> {
    validate_distro(distro)?;
    let ops = WslRemoteOps::new(distro);
    let source = BundledServerBinary::discover();
    let confirm = install_confirm();
    let lock = install_lock(distro);
    let _held = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    Installer::with_source(&ops, &source, confirm.as_ref(), host_label(distro)).restart_daemon()?;
    Ok(())
}

/// Restart the distro's daemon after making sure the binary it launches is this
/// build's. The bundled server is already on this computer, so unlike SSH there
/// is nothing to download — the copy is the whole install.
pub fn replace_wsl_server(distro: &str) -> io::Result<()> {
    validate_distro(distro)?;
    let ops = WslRemoteOps::new(distro);
    let source = BundledServerBinary::discover();
    let confirm = install_confirm();
    let lock = install_lock(distro);
    let _held = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    Installer::with_source(&ops, &source, confirm.as_ref(), host_label(distro)).replace()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::install::{InstallConfirm, InstallDecision, InstallRequest};
    use std::collections::BTreeMap;
    use std::sync::Mutex as StdMutex;

    #[test]
    fn real_distro_names_are_accepted() {
        for name in [
            "Ubuntu",
            "Ubuntu-22.04",
            "Ubuntu-24.04",
            "openSUSE-Leap-15.5",
            "kali-linux",
            "Arch",
            "my dev box",
            "NixOS",
            "Debian",
            "FedoraLinux-42",
            "开发环境",
        ] {
            assert_eq!(validate_distro(name), Ok(()), "{name:?} is a real name");
        }
    }

    #[test]
    fn names_that_could_not_be_passed_safely_are_refused() {
        assert_eq!(validate_distro(""), Err(DistroNameError::Empty));
        assert!(matches!(
            validate_distro("--shutdown"),
            Err(DistroNameError::LeadingDash(_))
        ));
        assert!(matches!(
            validate_distro("-d"),
            Err(DistroNameError::LeadingDash(_))
        ));
        assert!(matches!(
            validate_distro("Ubu\0ntu"),
            Err(DistroNameError::Control(_))
        ));
        assert!(matches!(
            validate_distro("Ubuntu\nArch"),
            Err(DistroNameError::Control(_))
        ));
        assert!(matches!(
            validate_distro(&"x".repeat(MAX_DISTRO_NAME + 1)),
            Err(DistroNameError::TooLong(_))
        ));
    }

    #[test]
    fn the_transport_command_line_is_the_designs() {
        assert_eq!(
            wsl_args("Ubuntu", &["tty7-server", "--stdio"]),
            vec!["-d", "Ubuntu", "--", "tty7-server", "--stdio"]
        );
        assert_eq!(
            wsl_args(
                "Ubuntu-22.04",
                &[
                    "/home/me/.local/share/tty7/bin/tty7-server-26.7.5",
                    "--stdio"
                ]
            ),
            vec![
                "-d",
                "Ubuntu-22.04",
                "--",
                "/home/me/.local/share/tty7/bin/tty7-server-26.7.5",
                "--stdio",
            ]
        );
        assert_eq!(wsl_args("Ubuntu", &[]), vec!["-d", "Ubuntu", "--"]);

        assert!(
            !wsl_args("Ubuntu", &["sh", "-s"])
                .iter()
                .any(|a| a == "--cd")
        );

        let args = wsl_args("Ubuntu", &["sh", "-s"]);
        assert!(
            args.iter().position(|a| a == "Ubuntu").unwrap()
                < args.iter().position(|a| a == "--").unwrap()
        );
    }

    #[test]
    fn the_host_label_is_the_connection_key() {
        for distro in ["Ubuntu", "Ubuntu-22.04", "my dev box"] {
            let target = crate::core::session::RemoteTarget::Wsl {
                distro: distro.to_string(),
            };
            assert_eq!(host_label(distro), target.connection_key());
        }
    }

    fn utf16le(s: &str, bom: bool) -> Vec<u8> {
        let mut out = Vec::new();
        if bom {
            out.extend_from_slice(&[0xff, 0xfe]);
        }
        for unit in s.encode_utf16() {
            out.extend_from_slice(&unit.to_le_bytes());
        }
        out
    }

    #[test]
    fn both_encodings_on_the_pipe_decode() {
        assert_eq!(decode_wsl_text(&utf16le("Ubuntu\r\n", true)), "Ubuntu\r\n");
        assert_eq!(decode_wsl_text(&utf16le("Ubuntu\r\n", false)), "Ubuntu\r\n");
        assert_eq!(decode_wsl_text(b"/home/me\n"), "/home/me\n");
        assert_eq!(decode_wsl_text(b""), "");
        assert_eq!(decode_wsl_text("héllo".as_bytes()), "héllo");
        assert_eq!(decode_wsl_text(&utf16le("héllo", false)), "héllo");
        assert_eq!(decode_wsl_text("\u{feff}ok".as_bytes()), "ok");
    }

    #[cfg(unix)]
    fn real_sh(script: &str) -> (String, bool) {
        use std::io::Write as _;
        let mut child = std::process::Command::new("sh")
            .arg("-s")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("every unix has /bin/sh");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(script.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            out.status.success(),
        )
    }

    #[cfg(unix)]
    fn sh_scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tty7-wsl-sh-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(unix)]
    fn fake_server(dir: &std::path::Path, name: &str, code: u8) -> String {
        use std::os::unix::fs::PermissionsExt as _;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\nexit {code}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.to_str().unwrap().to_string()
    }

    #[test]
    fn the_settle_asks_the_control_dialect_with_the_quoted_binary() {
        let script = settle_script("/home/a b/tty7-server-26.7.6", 4, "0.2", "0.3");
        assert!(
            script.contains("'/home/a b/tty7-server-26.7.6' --stdio --bridge < /dev/null"),
            "{script}"
        );
        assert!(!script.contains("--pane"), "{script}");
        assert!(!script.contains("-S "), "no file test: {script}");
        assert!(script.contains("-lt 4"), "{script}");
    }

    #[cfg(unix)]
    #[test]
    fn the_settle_stops_as_soon_as_the_daemon_answers() {
        let dir = sh_scratch("settle-answers");
        let serving = fake_server(&dir, "tty7-server-answers", 0);

        let started = std::time::Instant::now();
        let (out, ok) = real_sh(&settle_script(&serving, SETTLE_TRIES, SETTLE_STEP, "0.3"));
        let elapsed = started.elapsed();

        assert!(ok, "{out}");
        assert!(
            elapsed >= Duration::from_millis(250),
            "the grace after the answer did not happen: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_millis(1500),
            "it kept waiting after the daemon had answered: {elapsed:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_settle_gives_up_rather_than_hanging() {
        let dir = sh_scratch("settle-silent");
        let silent = fake_server(&dir, "tty7-server-silent", 1);

        let started = std::time::Instant::now();
        let (out, ok) = real_sh(&settle_script(&silent, 3, "0.1", "0.1"));
        let elapsed = started.elapsed();

        assert!(ok, "{out}");
        assert!(
            elapsed >= Duration::from_millis(350),
            "it did not ask three times: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "it never gave up: {elapsed:?}"
        );
    }

    #[test]
    fn the_settle_budget_fits_inside_the_launch_timeout() {
        let step: f64 = SETTLE_STEP.parse().expect("a number for `sleep`");
        let grace: f64 = SETTLE_GRACE.parse().expect("a number for `sleep`");
        let worst = f64::from(SETTLE_TRIES) * step + grace;
        assert!(
            worst < LAUNCH_TIMEOUT.as_secs_f64() / 2.0,
            "{worst}s of settle inside a {LAUNCH_TIMEOUT:?} launch budget"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_stat_script_really_runs_and_answers_correctly() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = sh_scratch("stat");

        let (out, ok) = real_sh(&stat_script(dir.to_str().unwrap()));
        assert!(ok, "{out}");
        assert!(parse_stat(&out).unwrap().unwrap().is_dir);

        let absent = dir.join("not-here");
        let (out, ok) = real_sh(&stat_script(absent.to_str().unwrap()));
        assert!(ok, "{out}");
        assert_eq!(parse_stat(&out).unwrap(), None);

        let plain = dir.join("plain");
        std::fs::write(&plain, b"0123456789").unwrap();
        let (out, ok) = real_sh(&stat_script(plain.to_str().unwrap()));
        assert!(ok, "{out}");
        let stat = parse_stat(&out).unwrap().unwrap();
        assert!(!stat.is_dir);
        assert_eq!(stat.size, 10);
        assert_eq!(stat.mode & 0o100, 0, "not executable yet");

        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (out, ok) = real_sh(&stat_script(plain.to_str().unwrap()));
        assert!(ok, "{out}");
        assert_ne!(parse_stat(&out).unwrap().unwrap().mode & 0o100, 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn the_list_script_really_runs_and_answers_correctly() {
        let dir = sh_scratch("list");

        let (out, ok) = real_sh(&list_script(dir.to_str().unwrap()));
        assert!(ok, "{out}");
        assert_eq!(parse_list(&out).unwrap(), Some(Vec::new()), "empty");

        let (out, ok) = real_sh(&list_script(dir.join("nope").to_str().unwrap()));
        assert!(ok, "{out}");
        assert_eq!(parse_list(&out).unwrap(), None, "absent");

        std::fs::write(dir.join("tty7-server-26.7.5"), b"x").unwrap();
        std::fs::write(dir.join(".tty7-server-26.7.5.tmp"), b"x").unwrap();
        std::fs::write(dir.join("a b c"), b"x").unwrap();
        let (out, ok) = real_sh(&list_script(dir.to_str().unwrap()));
        assert!(ok, "{out}");
        let mut got = parse_list(&out).unwrap().unwrap();
        got.sort();
        assert_eq!(
            got,
            vec![".tty7-server-26.7.5.tmp", "a b c", "tty7-server-26.7.5"],
            "`ls -A` includes dotfiles, and a space is not a separator"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn awkward_paths_survive_into_a_running_shell() {
        let dir = sh_scratch("quote").join("o'brien's dir");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("tty7-server-1.0.0");
        std::fs::write(&file, b"abc").unwrap();

        let (out, ok) = real_sh(&stat_script(file.to_str().unwrap()));
        assert!(ok, "{out}");
        assert_eq!(parse_stat(&out).unwrap().unwrap().size, 3);

        let (out, ok) = real_sh(&list_script(dir.to_str().unwrap()));
        assert!(ok, "{out}");
        assert_eq!(
            parse_list(&out).unwrap(),
            Some(vec!["tty7-server-1.0.0".to_string()])
        );

        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn the_home_script_really_reports_an_absolute_path() {
        let (out, ok) = real_sh(HOME_SCRIPT);
        assert!(ok, "{out}");
        let home = out.lines().find_map(|l| marked(l, "home=")).expect("home");
        assert!(home.starts_with('/'), "{home:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_redirected_command_does_not_consume_the_script_behind_it() {
        let script = format!("cat < /dev/null\nprintf '{MARKER} stat=file 755 42\\n'\n");
        let (out, ok) = real_sh(&script);
        assert!(ok, "{out}");
        assert_eq!(
            parse_stat(&out).unwrap().unwrap().size,
            42,
            "the line after a stdin-reading command must still run"
        );

        let probe = format!("{} --stdio --bridge < /dev/null\n", shell_quote("false"));
        let (_, ok) = real_sh(&probe);
        assert!(!ok, "a non-serving machine must exit non-zero");
        let probe = format!("{} --stdio --bridge < /dev/null\n", shell_quote("true"));
        let (_, ok) = real_sh(&probe);
        assert!(ok, "a serving machine must exit zero");
    }

    #[test]
    fn a_mixed_encoding_pipe_keeps_both_halves_readable() {
        let mut mixed = utf16le("wsl: Detected localhost proxy configuration\n", true);
        mixed.extend_from_slice(b"sh: 1: cannot create /home/me/x: Permission denied\n");
        let text = decode_wsl_text(&mixed);
        assert!(
            text.contains("Detected localhost proxy configuration"),
            "the wsl.exe half must survive: {text:?}"
        );
        assert!(
            text.contains("Permission denied"),
            "the distribution's half must survive: {text:?}"
        );

        let mut mixed = Vec::from(&b"__tty7_wsl__ home=/home/me\n"[..]);
        mixed.extend_from_slice(&utf16le("wsl: something happened\n", false));
        let text = decode_wsl_text(&mixed);
        assert_eq!(
            text.lines().find_map(|l| marked(l, "home=")),
            Some("/home/me"),
            "a marker line must not be garbled by a UTF-16 line beside it: {text:?}"
        );
        assert!(text.contains("something happened"), "{text:?}");
    }

    #[cfg(unix)]
    #[test]
    fn the_mutating_scripts_really_run() {
        let dir = sh_scratch("mutate");
        let nested = dir.join("a b").join("c'\''d");
        let q = |p: &Path| p.to_str().unwrap().to_string();

        let (_, ok) = real_sh(&format!("mkdir -p {}\n", shell_quote(&q(&nested))));
        assert!(ok);
        assert!(nested.is_dir(), "mkdir did not create {nested:?}");
        let (_, ok) = real_sh(&format!("mkdir -p {}\n", shell_quote(&q(&nested))));
        assert!(ok, "mkdir -p must be idempotent");

        let from = nested.join(".tty7-server-1.0.0.tmp");
        let to = nested.join("tty7-server-1.0.0");
        std::fs::write(&from, b"payload").unwrap();

        let (_, ok) = real_sh(&format!("chmod {:o} {}\n", 0o755, shell_quote(&q(&from))));
        assert!(ok);
        let (out, ok) = real_sh(&stat_script(&q(&from)));
        assert!(ok);
        assert_ne!(
            parse_stat(&out).unwrap().unwrap().mode & 0o100,
            0,
            "chmod did not take"
        );

        let (_, ok) = real_sh(&format!(
            "mv -f {} {}\n",
            shell_quote(&q(&from)),
            shell_quote(&q(&to))
        ));
        assert!(ok);
        assert!(to.is_file() && !from.exists(), "mv did not publish");

        let (_, ok) = real_sh(&format!("rm -f {}\n", shell_quote(&q(&to))));
        assert!(ok);
        assert!(!to.exists(), "rm did not remove");
        let (_, ok) = real_sh(&format!("rm -f {}\n", shell_quote(&q(&to))));
        assert!(ok, "rm -f must not fail on a missing file");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn scripts_run_from_the_home_directory() {
        let (out, ok) = real_sh(&format!("{CD_HOME}pwd\n"));
        assert!(ok, "{out}");
        let home = std::env::var("HOME").unwrap();
        assert_eq!(out.trim(), home.trim_end_matches('/'), "cd did not land");

        let (out, ok) = real_sh(&format!("HOME=/no/such/place\n{CD_HOME}pwd\n"));
        assert!(ok, "{out}");
        assert_eq!(out.trim(), "/", "the fallback must be `/`, not a failure");
    }

    #[test]
    fn the_distro_list_is_decoded_and_filtered() {
        let raw = utf16le(
            "Ubuntu-22.04\r\ndocker-desktop\r\ndocker-desktop-data\r\nArch\r\n\r\n",
            true,
        );
        assert_eq!(parse_distro_list(&raw), vec!["Ubuntu-22.04", "Arch"]);
        assert!(parse_distro_list(&[]).is_empty());
        let padded = utf16le("Ubuntu\0\r\n", true);
        assert_eq!(parse_distro_list(&padded), vec!["Ubuntu"]);
    }

    #[test]
    fn script_paths_are_shell_quoted() {
        let script = stat_script("/home/o'brien/my dir/tty7-server-1.0.0");
        assert!(
            script.contains(r"p='/home/o'\''brien/my dir/tty7-server-1.0.0'"),
            "{script}"
        );
        let script = list_script("/home/a b/bin");
        assert!(script.contains("d='/home/a b/bin'"), "{script}");
        assert!(!list_script("/x").contains("\"/x\""));
    }

    #[test]
    fn the_stat_probe_round_trips_its_three_answers() {
        assert_eq!(parse_stat("__tty7_wsl__ stat=none 0 0\n").unwrap(), None);

        let dir = parse_stat("__tty7_wsl__ stat=dir 0 0\n").unwrap().unwrap();
        assert!(dir.is_dir);

        let exe = parse_stat("__tty7_wsl__ stat=file 755 6291456\n")
            .unwrap()
            .unwrap();
        assert!(!exe.is_dir);
        assert_eq!(exe.size, 6_291_456);
        assert_ne!(exe.mode & 0o100, 0, "must read as executable");

        let half = parse_stat("__tty7_wsl__ stat=file 644 6291456\n")
            .unwrap()
            .unwrap();
        assert_eq!(half.mode & 0o100, 0, "must read as not executable");
    }

    #[test]
    fn the_stat_probe_ignores_noise_but_not_silence() {
        let noisy = "Welcome to Ubuntu 22.04\n__tty7_wsl__ stat=file 755 10\nlast login: …\n";
        assert_eq!(parse_stat(noisy).unwrap().unwrap().size, 10);
        assert!(parse_stat("").is_err());
        assert!(parse_stat("bash: sh: command not found\n").is_err());
        assert!(parse_stat("__tty7_wsl__ stat=weird 0 0\n").is_err());
    }

    #[test]
    fn a_listing_distinguishes_empty_from_absent() {
        assert_eq!(parse_list("__tty7_wsl__ nolisting\n").unwrap(), None);
        assert_eq!(
            parse_list("__tty7_wsl__ listing\n").unwrap(),
            Some(Vec::new())
        );
        assert_eq!(
            parse_list(
                "__tty7_wsl__ listing\n\
                 __tty7_wsl__ entry=tty7-server-26.7.5\n\
                 __tty7_wsl__ entry=.tty7-server-26.7.5.tmp\n"
            )
            .unwrap(),
            Some(vec![
                "tty7-server-26.7.5".to_string(),
                ".tty7-server-26.7.5.tmp".to_string()
            ])
        );
        assert_eq!(
            parse_list("__tty7_wsl__ listing\n__tty7_wsl__ entry=a b c\n").unwrap(),
            Some(vec!["a b c".to_string()])
        );
        assert!(parse_list("nothing at all\n").is_err());
    }

    #[test]
    fn the_search_order_is_specific_first_and_deduplicated() {
        let exe = PathBuf::from("/opt/tty7/bin/tty7-app.exe");
        let dirs = bundled_search_dirs(Some(&exe), None);
        assert_eq!(dirs[0], PathBuf::from("/opt/tty7/bin/server"));
        assert_eq!(dirs[1], PathBuf::from("/opt/tty7/bin"));
        assert!(dirs.contains(&PathBuf::from("/opt/tty7/Resources/server")));

        let over = PathBuf::from("/tmp/mine");
        let dirs = bundled_search_dirs(Some(&exe), Some(&over));
        assert_eq!(dirs[0], over, "the override outranks everything");

        let same = PathBuf::from("/opt/tty7/bin/server");
        let dirs = bundled_search_dirs(Some(&exe), Some(&same));
        assert_eq!(dirs.iter().filter(|d| **d == same).count(), 1);

        assert!(bundled_search_dirs(None, None).is_empty());
        assert_eq!(bundled_search_dirs(None, Some(&over)), vec![over]);
    }

    #[test]
    fn a_missing_bundled_binary_names_every_place_it_looked() {
        let tmp = std::env::temp_dir().join(format!("tty7-wsl-src-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let source = BundledServerBinary::in_dirs(vec![tmp.join("nope"), tmp.clone()]);
        let err = source
            .load("26.7.5", super::super::asset::ASSET_X86_64)
            .expect_err("nothing bundled yet");
        let msg = err.to_string();
        assert!(msg.contains("tty7-server-linux-x86_64-musl"), "{msg}");
        assert!(msg.contains(&tmp.display().to_string()), "{msg}");
        assert!(matches!(err, InstallError::MissingBundled { .. }));

        let path = tmp.join(super::super::asset::ASSET_X86_64);
        std::fs::write(&path, b"").unwrap();
        let err = source
            .load("26.7.5", super::super::asset::ASSET_X86_64)
            .expect_err("empty is not a binary");
        assert!(err.to_string().contains("empty file"), "{err}");

        std::fs::write(&path, b"\x7fELF-not-really").unwrap();
        let loaded = source
            .load("26.7.5", super::super::asset::ASSET_X86_64)
            .unwrap();
        assert_eq!(loaded.bytes, b"\x7fELF-not-really");
        assert_eq!(loaded.origin, path.display().to_string());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[derive(Default)]
    struct FakeDistro {
        files: StdMutex<BTreeMap<String, (Vec<u8>, u32)>>,
        dirs: StdMutex<Vec<String>>,
        commands: StdMutex<Vec<String>>,
        serving: StdMutex<bool>,
    }

    impl FakeDistro {
        fn ran(&self) -> Vec<String> {
            self.commands.lock().unwrap().clone()
        }
    }

    impl RemoteOps for FakeDistro {
        fn home_dir(&self) -> Result<String, String> {
            Ok("/home/me".to_string())
        }

        fn run(&self, cmd: &str) -> Result<ExecOutput, String> {
            self.commands.lock().unwrap().push(cmd.to_string());
            let ok = |stdout: &str| {
                Ok(ExecOutput {
                    status: Some(0),
                    stdout: stdout.to_string(),
                    stderr: String::new(),
                })
            };
            if cmd.contains("uname -sm") {
                return ok("Linux x86_64\n");
            }
            if cmd.contains("--stdio --bridge") {
                return if *self.serving.lock().unwrap() {
                    ok("")
                } else {
                    Ok(ExecOutput {
                        status: Some(1),
                        stdout: String::new(),
                        stderr: String::new(),
                    })
                };
            }
            if let Some(exe) = cmd
                .trim()
                .strip_suffix(crate::daemon::install::PROTOCOL_FLAG)
            {
                let exe = exe.trim().trim_matches('\'');
                return if self.files.lock().unwrap().contains_key(exe) {
                    ok(&crate::daemon::install::RemoteProtocol::of_this_build().to_line())
                } else {
                    Ok(ExecOutput {
                        status: Some(1),
                        stdout: String::new(),
                        stderr: "unknown flag".into(),
                    })
                };
            }
            ok("")
        }

        fn spawn_detached(&self, cmd: &str) -> Result<(), String> {
            self.commands.lock().unwrap().push(cmd.to_string());
            *self.serving.lock().unwrap() = true;
            Ok(())
        }

        fn stat(&self, path: &str) -> Result<Option<RemoteStat>, String> {
            if self.dirs.lock().unwrap().iter().any(|d| d == path) {
                return Ok(Some(RemoteStat {
                    size: 0,
                    mode: 0o755,
                    is_dir: true,
                }));
            }
            Ok(self
                .files
                .lock()
                .unwrap()
                .get(path)
                .map(|(bytes, mode)| RemoteStat {
                    size: bytes.len() as u64,
                    mode: *mode,
                    is_dir: false,
                }))
        }

        fn mkdir(&self, path: &str) -> Result<(), String> {
            let mut dirs = self.dirs.lock().unwrap();
            if !dirs.iter().any(|d| d == path) {
                dirs.push(path.to_string());
            }
            Ok(())
        }

        fn chmod(&self, path: &str, mode: u32) -> Result<(), String> {
            if let Some(entry) = self.files.lock().unwrap().get_mut(path) {
                entry.1 = mode;
            }
            Ok(())
        }

        fn put(&self, path: &str, bytes: &[u8]) -> Result<(), String> {
            self.files
                .lock()
                .unwrap()
                .insert(path.to_string(), (bytes.to_vec(), 0o644));
            Ok(())
        }

        fn rename(&self, from: &str, to: &str) -> Result<(), String> {
            let mut files = self.files.lock().unwrap();
            let entry = files.remove(from).ok_or("no such file")?;
            files.insert(to.to_string(), entry);
            Ok(())
        }

        fn remove_file(&self, path: &str) -> Result<(), String> {
            self.files.lock().unwrap().remove(path);
            Ok(())
        }

        fn list_dir(&self, path: &str) -> Result<Option<Vec<String>>, String> {
            if !self.dirs.lock().unwrap().iter().any(|d| d == path) {
                return Ok(None);
            }
            let prefix = format!("{path}/");
            Ok(Some(
                self.files
                    .lock()
                    .unwrap()
                    .keys()
                    .filter_map(|k| k.strip_prefix(&prefix).map(str::to_string))
                    .collect(),
            ))
        }
    }

    struct Approve(StdMutex<Vec<InstallRequest>>);
    impl InstallConfirm for Approve {
        fn confirm(&self, request: &InstallRequest) -> InstallDecision {
            self.0.lock().unwrap().push(request.clone());
            InstallDecision::Approve
        }
    }

    struct Decline;
    impl InstallConfirm for Decline {
        fn confirm(&self, _: &InstallRequest) -> InstallDecision {
            InstallDecision::Decline
        }
    }

    fn bundled(dir: &Path, bytes: &[u8]) -> BundledServerBinary {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(super::super::asset::ASSET_X86_64), bytes).unwrap();
        BundledServerBinary::in_dirs(vec![dir.to_path_buf()])
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tty7-wsl-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_wsl_install_copies_the_bundled_binary_and_launches_a_daemon() {
        let dir = scratch("install");
        let source = bundled(&dir, b"\x7fELF pretend server");
        let ops = FakeDistro::default();
        let confirm = Approve(StdMutex::new(Vec::new()));

        let report = Installer::with_source(&ops, &source, &confirm, host_label("Ubuntu"))
            .with_version("26.7.5")
            .with_timeouts(Duration::from_millis(200), Duration::from_millis(5))
            .run()
            .expect("installs");

        assert!(report.installed);
        assert!(report.launched);
        assert!(report.confirmed, "a first install asks");
        let dialect = crate::daemon::install::RemoteProtocol::of_this_build();
        assert_eq!(
            report.paths.binary,
            format!(
                "/home/me/.local/share/tty7/bin/tty7-server-c{}p{}",
                dialect.control, dialect.protocol
            ),
            "the name follows the dialect, not the `--with_version` release"
        );
        assert_eq!(
            ops.files.lock().unwrap()[&report.paths.binary].0,
            b"\x7fELF pretend server"
        );
        assert_ne!(
            ops.files.lock().unwrap()[&report.paths.binary].1 & 0o100,
            0,
            "published executable"
        );
        assert!(
            !ops.files.lock().unwrap().contains_key(&report.paths.temp),
            "the temp file is renamed away, not left behind"
        );

        let asked = confirm.0.lock().unwrap();
        assert_eq!(asked.len(), 1);
        assert_eq!(
            asked[0].source_url,
            dir.join(super::super::asset::ASSET_X86_64)
                .display()
                .to_string()
        );
        assert_eq!(asked[0].host, "wsl:Ubuntu");
        assert_eq!(asked[0].size_bytes, b"\x7fELF pretend server".len() as u64);
        assert!(!asked[0].source_url.starts_with("https://"), "no download");

        assert!(
            ops.ran()
                .iter()
                .any(|c| c.contains("--stdio --bridge") && c.contains("/dev/null")),
            "{:?}",
            ops.ran()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_second_connect_installs_nothing_and_asks_nothing() {
        let dir = scratch("idempotent");
        let source = bundled(&dir, b"\x7fELF pretend server");
        let ops = FakeDistro::default();
        let confirm = Approve(StdMutex::new(Vec::new()));
        let install = || {
            Installer::with_source(&ops, &source, &confirm, host_label("Ubuntu"))
                .with_version("26.7.5")
                .with_timeouts(Duration::from_millis(200), Duration::from_millis(5))
                .run()
                .expect("installs")
        };

        install();
        let again = install();
        assert!(!again.installed, "already there");
        assert!(!again.launched, "already serving");
        assert!(!again.confirmed);
        assert_eq!(confirm.0.lock().unwrap().len(), 1, "asked exactly once");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn declining_writes_nothing_into_the_distribution() {
        let dir = scratch("declined");
        let source = bundled(&dir, b"\x7fELF pretend server");
        let ops = FakeDistro::default();

        let err = Installer::with_source(&ops, &source, &Decline, host_label("Ubuntu"))
            .with_version("26.7.5")
            .run()
            .expect_err("declined");
        assert!(matches!(err, InstallError::Declined { .. }), "{err}");
        assert!(ops.files.lock().unwrap().is_empty(), "nothing was written");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_client_with_no_bundled_binary_says_so() {
        let dir = scratch("nobinary");
        let source = BundledServerBinary::in_dirs(vec![dir.clone()]);
        let ops = FakeDistro::default();
        let confirm = Approve(StdMutex::new(Vec::new()));

        let err = Installer::with_source(&ops, &source, &confirm, host_label("Ubuntu"))
            .with_version("26.7.5")
            .run()
            .expect_err("nothing to install");
        assert!(matches!(err, InstallError::MissingBundled { .. }), "{err}");
        let msg = err.to_string();
        assert!(msg.contains("does not ship a Linux server binary"), "{msg}");
        assert!(msg.contains(&dir.display().to_string()), "{msg}");
        assert!(
            confirm.0.lock().unwrap().is_empty(),
            "nobody is asked about an install that cannot happen"
        );
    }

    #[test]
    fn the_bundled_asset_follows_the_distributions_architecture() {
        struct Arm(FakeDistro);
        impl RemoteOps for Arm {
            fn run(&self, cmd: &str) -> Result<ExecOutput, String> {
                if cmd.contains("uname -sm") {
                    return Ok(ExecOutput {
                        status: Some(0),
                        stdout: "Linux aarch64\n".to_string(),
                        stderr: String::new(),
                    });
                }
                self.0.run(cmd)
            }
            fn home_dir(&self) -> Result<String, String> {
                self.0.home_dir()
            }
            fn spawn_detached(&self, cmd: &str) -> Result<(), String> {
                self.0.spawn_detached(cmd)
            }
            fn stat(&self, path: &str) -> Result<Option<RemoteStat>, String> {
                self.0.stat(path)
            }
            fn mkdir(&self, path: &str) -> Result<(), String> {
                self.0.mkdir(path)
            }
            fn chmod(&self, path: &str, mode: u32) -> Result<(), String> {
                self.0.chmod(path, mode)
            }
            fn put(&self, path: &str, bytes: &[u8]) -> Result<(), String> {
                self.0.put(path, bytes)
            }
            fn rename(&self, from: &str, to: &str) -> Result<(), String> {
                self.0.rename(from, to)
            }
            fn remove_file(&self, path: &str) -> Result<(), String> {
                self.0.remove_file(path)
            }
            fn list_dir(&self, path: &str) -> Result<Option<Vec<String>>, String> {
                self.0.list_dir(path)
            }
        }

        let dir = scratch("arm");
        let source = bundled(&dir, b"\x7fELF x86");
        let ops = Arm(FakeDistro::default());
        let confirm = Approve(StdMutex::new(Vec::new()));
        let err = Installer::with_source(&ops, &source, &confirm, host_label("Ubuntu"))
            .with_version("26.7.5")
            .run()
            .expect_err("no aarch64 binary bundled");
        let msg = err.to_string();
        assert!(msg.contains("linux-aarch64-musl"), "{msg}");

        std::fs::write(dir.join(super::super::asset::ASSET_AARCH64), b"\x7fELF arm").unwrap();
        let source = BundledServerBinary::in_dirs(vec![dir.clone()]);
        let report = Installer::with_source(&ops, &source, &confirm, host_label("Ubuntu"))
            .with_version("26.7.5")
            .with_timeouts(Duration::from_millis(200), Duration::from_millis(5))
            .run()
            .expect("installs the arm binary");
        assert_eq!(
            ops.0.files.lock().unwrap()[&report.paths.binary].0,
            b"\x7fELF arm"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_install_lock_is_shared_per_distro_and_not_across_them() {
        let a1 = install_lock("test-lock-a");
        let a2 = install_lock("test-lock-a");
        let b = install_lock("test-lock-b");
        assert!(Arc::ptr_eq(&a1, &a2), "same distro must share one lock");
        assert!(!Arc::ptr_eq(&a1, &b), "different distros must not block");

        let held = a1.lock().unwrap();
        assert!(a2.try_lock().is_err(), "the lock must actually exclude");
        assert!(b.try_lock().is_ok(), "another distro is unaffected");
        drop(held);
        assert!(a2.try_lock().is_ok());
    }

    #[test]
    fn ensure_refuses_an_unusable_distro_name_before_spawning_anything() {
        let err = ensure_wsl_server("--shutdown").expect_err("refused");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("leading `-`"), "{err}");

        let err = restart_wsl_daemon("").expect_err("refused");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
}

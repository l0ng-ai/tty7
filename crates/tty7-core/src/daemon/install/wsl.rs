//! WSL — a distribution on *this* machine as a remote workspace host
//! (decision D9).
//!
//! ## Why WSL is its own transport instead of "just another SSH host"
//!
//! D9: requiring the user to install and configure an `sshd` inside a
//! distribution that is already running on their own computer is absurd, and it
//! also breaks the automatic-install story — there would be nothing to install
//! *onto* until the user had already done the hard part by hand. So a WSL host
//! is reached by spawning `wsl.exe -d <distro> -- <server> --stdio` and treating
//! that child's stdin/stdout as the link. **No SSH, no authentication, no
//! network, no host key, no port.**
//!
//! ## The layering, and where the untestable part is confined
//!
//! | Layer | What it is | Tested here |
//! |---|---|---|
//! | Command construction | [`wsl_args`], the `*_script` builders, [`shell_quote`] | ✅ pure |
//! | Output decoding | [`decode_wsl_text`], [`parse_distro_list`], [`parse_stat`], [`parse_list`] | ✅ pure |
//! | Binary discovery | [`bundled_search_dirs`], [`BundledServerBinary`] | ✅ against a temp dir |
//! | The install state machine | [`super::Installer`], shared verbatim with SSH | ✅ against a fake [`RemoteOps`] |
//! | Actually spawning `wsl.exe` | [`WslRemoteOps`]'s `invoke` | ❌ needs Windows + WSL |
//!
//! Everything above the last row runs in this crate's test suite on any OS. The
//! last row is deliberately the thinnest thing that could work: build an argv,
//! spawn, feed stdin, read stdout, decode. It is also **the only part that has
//! never been executed** — see the module's tests for exactly which strings the
//! untested layer is expected to produce.
//!
//! Note that none of this is `#[cfg(windows)]`. `wsl.exe` is spawned by name
//! like any other program, so every line here compiles (and the pure parts run)
//! on macOS and Linux; on those systems the spawn simply fails with "not found",
//! which is the honest answer.
//!
//! ## Two decisions worth writing down
//!
//! **Scripts travel on stdin, not on the command line.** Every probe runs as
//! `wsl.exe -d <distro> -- sh -s` with the script written to the child's stdin.
//! The alternative, `sh -c "<script>"`, would push a shell script through two
//! separate quoting layers — Rust's Windows command-line quoting on the way out,
//! and `wsl.exe`'s own command-line reconstruction on the way in — for a string
//! full of quotes, `$`, and newlines. `-s` reduces the argv to five fixed,
//! space-free words, so there is nothing left for either layer to get wrong.
//!
//! **The binary is written with `tee`, not through `\\wsl$`.** See
//! [`WslRemoteOps::put`].

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

/// The launcher. Resolved through `PATH` rather than `%SystemRoot%\System32`:
/// that is where it lives, and hard-coding a system path buys nothing over
/// letting the OS answer.
pub const WSL_EXE: &str = "wsl.exe";

/// Prefix on every line this module's probes emit, so a distribution whose
/// startup prints anything of its own cannot be mistaken for an answer. Same
/// tactic as [`super::super::remote_link::REMOTE_ENV_PROBE`].
const MARKER: &str = "__tty7_wsl__";

/// Budget for one probe. Generous, because the *first* command against a
/// stopped distribution pays for booting its VM and init.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
/// Budget for the daemon launch. It backgrounds and returns, but not
/// immediately — see [`launch_settle`], which holds the invocation open until the
/// daemon answers.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(30);
/// Budget for writing the server binary — a few megabytes over a pipe, plus
/// whatever the distribution's disk is doing.
const PUT_TIMEOUT: Duration = Duration::from_secs(180);

// ---------------------------------------------------------------------------
// Distribution names.
// ---------------------------------------------------------------------------

/// Why a string cannot be used as a `wsl.exe -d` argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DistroNameError {
    Empty,
    /// A name starting with `-` would be read as an option by `wsl.exe`, not as
    /// a distribution — including, in the worst case, one that does something.
    LeadingDash(String),
    /// Control characters (NUL especially) cannot survive the trip through a
    /// Windows command line intact, and a name containing one did not come from
    /// `wsl.exe -l -q`.
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

/// Longest name we will pass along. `wsl.exe` itself has no documented limit;
/// this exists so a corrupted config cannot hand a megabyte to `CreateProcess`.
const MAX_DISTRO_NAME: usize = 255;

/// Whether `name` can be handed to `wsl.exe -d`.
///
/// Deliberately permissive about everything else — spaces, dots, non-ASCII and
/// `+` all occur in real distribution names (`Ubuntu-22.04`, `openSUSE-Leap-15.5`,
/// names the user typed when importing a tarball) and they are passed as a
/// single argv element, never through a shell.
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

/// The full argument list for `wsl.exe`, running `argv` inside `distro`.
///
/// `--` is mandatory, not decoration: without it `wsl.exe` keeps parsing options
/// and a command whose first word happens to start with `-` would be swallowed.
///
/// **Deliberately no `--cd ~`.** It would be the tidy way to run everything from
/// the distribution's home — which is where an SSH session channel already
/// starts, and it matters, because `wsl.exe` otherwise runs the command in the
/// *Windows* working directory translated across the mount (`C:\Users\me` →
/// `/mnt/c/Users/me`) and the daemon never `chdir`s, so a process that outlives
/// every window would sit on a drvfs handle for as long as it ran.
///
/// But `--cd` only exists in the Store/preview `wsl.exe` (0.51.2+); the inbox one
/// on Windows 10 rejects it, and an unknown option makes `wsl.exe` fail *before*
/// the distribution starts. That turns a cosmetic problem into total breakage on
/// a large set of machines. The working directory is fixed inside the shell
/// instead — see [`CD_HOME`], which costs nothing and needs no `wsl.exe`
/// features.
pub fn wsl_args(distro: &str, argv: &[&str]) -> Vec<String> {
    let mut args = vec!["-d".to_string(), distro.to_string(), "--".to_string()];
    args.extend(argv.iter().map(|a| (*a).to_string()));
    args
}

/// The machine label a WSL distribution is known by, matching
/// [`RemoteTarget::connection_key`](crate::core::session::RemoteTarget) so a
/// consent prompt, a mismatch record and a session all name the same thing.
pub fn host_label(distro: &str) -> String {
    format!("wsl:{distro}")
}

// ---------------------------------------------------------------------------
// Enumerating distributions — the entry point the "connect to a host" UI needs.
// ---------------------------------------------------------------------------

/// Installed WSL distributions, or empty when WSL is not installed (or this is
/// not Windows).
///
/// Blocking and cheap-ish, but it does spawn a process: call it off the UI
/// thread. Empty is a normal answer and never an error — "no WSL here" is
/// exactly what a Mac or a Windows box without WSL should report.
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

/// Decode `wsl.exe -l -q` output into distribution names.
///
/// `wsl.exe` writes UTF-16LE, one name per line, and Docker Desktop's two
/// internal distributions are filtered out: they are not user environments,
/// they have no shell worth landing in, and offering them as workspace hosts
/// only ever produces a confusing failure.
pub fn parse_distro_list(bytes: &[u8]) -> Vec<String> {
    decode_wsl_text(bytes)
        .lines()
        .map(|line| line.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}' || c == '\0'))
        .filter(|line| !line.is_empty() && !line.starts_with("docker-desktop"))
        .map(str::to_string)
        .collect()
}

/// Decode bytes that came from a WSL-related process.
///
/// `wsl.exe`'s *own* output — the distribution list, and its error messages — is
/// UTF-16LE, while anything produced *inside* the distribution is UTF-8. Both
/// arrive on the same pipes, so the encoding has to be sniffed rather than
/// assumed: a NUL byte is impossible in valid UTF-8 text and unavoidable in
/// UTF-16LE ASCII, which makes it a reliable discriminator in exactly this
/// setting.
/// **Line by line**, because the two encodings really do share a pipe. The
/// mixing happens on stderr: `wsl.exe` prepends its own warnings (the
/// "Detected localhost proxy configuration" notice is emitted on essentially
/// every invocation on affected hosts) to whatever the distribution's shell then
/// writes. Decoding the whole buffer by one verdict would turn the *other*
/// half into mojibake — and on stderr that half is the diagnostic
/// [`ExecOutput::failure_reason`] is about to show the user.
///
/// A newline is `0A` in UTF-8 and `0A 00` in UTF-16LE, so splitting on `0A`
/// separates lines under either encoding and leaves at most one stray `00` at
/// the head of the next line, which is dropped before the per-line verdict.
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
    // The low half of the `0A 00` that ended the previous UTF-16 line.
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

// ---------------------------------------------------------------------------
// The scripts, and how their answers are read back.
// ---------------------------------------------------------------------------

/// `$HOME` as the distribution's default user sees it. Not guessed from the
/// distro name, and not `\\wsl$\<distro>\home\<user>` with `<user>` inferred —
/// the default user is configurable per distribution (`/etc/wsl.conf`) and is
/// routinely not the one whose name appears anywhere Windows can see.
const HOME_SCRIPT: &str = "printf '__tty7_wsl__ home=%s\\n' \"$HOME\"\n";

/// Prefixed to every script, because `wsl.exe` starts the command in the
/// translated Windows working directory (`/mnt/c/…`) and
/// [`wsl_args`] explains why `--cd ~` cannot be used to fix that.
///
/// The one that really matters is the daemon launch: `tty7-server --daemon` never
/// `chdir`s, so without this it would hold a drvfs handle open for its entire
/// life — and its life is "until the machine reboots". `|| cd /` covers a
/// distribution whose default user has no home directory.
const CD_HOME: &str = "cd \"$HOME\" 2>/dev/null || cd /\n";

/// How many times the settle asks the daemon whether it is serving, and how long
/// it waits between asks — a little over five seconds in all, generous for a
/// distribution that is booting its VM, and far inside [`LAUNCH_TIMEOUT`].
const SETTLE_TRIES: u32 = 25;
const SETTLE_STEP: &str = "0.2";
/// How long to hold the invocation open *after* the daemon answers. See
/// [`launch_settle`].
const SETTLE_GRACE: &str = "0.3";

/// The wait that runs after the daemon launch, in the same `wsl.exe`
/// invocation, so `wsl.exe` does not exit while the daemon is still detaching.
///
/// **WSL reaps what an interop session started when that session's `wsl.exe`
/// exits**, and `setsid` does not make the child safe the instant it runs.
/// [`launch_command`](super::launch_command) backgrounds the daemon and returns
/// in milliseconds, so without this the sequence is: `sh` exits, `wsl.exe`
/// exits, WSL tears the session down, and the daemon dies before it can bind.
/// What the user sees is [`ensure_daemon`](super::Installer::ensure_daemon)'s
/// "started but nothing was answering on the control socket", with a correctly
/// installed binary and nothing else to go on.
///
/// Reproduced by hand against `Ubuntu-24.04`, running the launch line by
/// itself: with nothing after it the daemon is gone; with as little as
/// `sleep 0.3` after it the daemon lives and serves. Nothing about the launch
/// line itself is wrong — `nohup`, `setsid --fork` and a double-forked subshell
/// all die the same way — so the wait is the fix, not a different spelling of
/// the detach.
///
/// The SSH path needs none of this (an exec channel's close is unhurried), which
/// is why the shared [`launch_command`](super::launch_command) stays as it is
/// and this is a [`RemoteOps::launch_settle`](super::RemoteOps::launch_settle)
/// instead.
///
/// # It waits for an answer, not for a fixed number of seconds
///
/// A flat `sleep` is a bet on how long a distribution takes to detach, and the
/// one that loses that bet is the cold or loaded distribution — the same
/// condition the failure needed in the first place. So the wait asks the daemon
/// the only question that settles it, `--stdio --bridge`, exactly as
/// [`ensure_daemon`](super::Installer::ensure_daemon) asks it from this side, and
/// stops as soon as it is answered. The normal case is therefore *shorter* than
/// the flat second it replaces, and the cold case is allowed the seconds it
/// actually needs instead of failing with nothing to go on.
///
/// **Waiting on `<config dir>/daemon.sock` would be wrong**, which is why this
/// waits on the answer and not on the file.
/// [`ensure_daemon`](super::Installer::ensure_daemon) only reaches the launch
/// when the socket did *not* answer, and the ordinary reason for that is a socket
/// file a dead daemon left behind — `wsl --shutdown` is a routine thing to run.
/// A `-S` test would pass on that stale file immediately, skip the wait, and
/// restore the bug in exactly the state that produced it. A daemon that answers
/// cannot be a leftover file.
///
/// [`SETTLE_GRACE`] follows the answer because what makes the daemon safe is not
/// observable from here — 0.3s of *any* wait was enough in the reproduction, so
/// the answer is followed by that much regardless.
pub(super) fn launch_settle(binary: &str) -> String {
    settle_script(binary, SETTLE_TRIES, SETTLE_STEP, SETTLE_GRACE)
}

/// [`launch_settle`] with its budget spelled out, so a test can run the real
/// script against a real `sh` without waiting out the real budget.
fn settle_script(binary: &str, tries: u32, step: &str, grace: &str) -> String {
    let bin = shell_quote(binary);
    // `< /dev/null` on the probe matters twice over: it is what makes `--bridge`
    // answer and exit rather than proxy, and this script itself arrives on the
    // shell's stdin, so a probe left reading stdin would eat the rest of it.
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

/// `HOME_SCRIPT` has to spell the marker out, because a `const` cannot
/// `format!`. This is the guard that a rename of [`MARKER`] cannot slip past:
/// it fails at compile time, not on a distribution nobody can reach from CI.
const _: () = assert!(
    konst_contains(HOME_SCRIPT, MARKER),
    "HOME_SCRIPT must carry MARKER, or `home_dir` silently stops parsing"
);

/// `str::contains` is not `const`; this is the same search, spelled so it can run
/// in a `const` block.
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

/// `stat`, reduced to the three facts the installer actually decides on: is it a
/// directory, is it executable, how big is it.
///
/// Built from `test` and `wc` rather than `stat(1)`, whose format flags differ
/// between GNU coreutils and BusyBox — and a minimal distribution image is
/// exactly where an install is most likely to be attempted.
///
/// The reported mode is therefore a *reconstruction*: `0755` for something
/// executable, `0644` for something that is not. That is all
/// [`Installer::run`](super::Installer::run) reads it for (`mode & 0o100`), and
/// claiming more precision than the probe has would be a lie in a struct field.
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

/// Read [`stat_script`]'s answer. `Ok(None)` is "not there", which is the
/// expected answer on a first install and must never surface as an error.
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

/// `ls -A` on one directory, one marked line per entry, plus a marker saying
/// whether the directory was there at all — the two answers have to be
/// distinguishable, because "empty" and "absent" mean the same thing to the
/// first-install prompt but not to anything else.
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

/// The payload of a marked line, once past the marker and an optional key.
fn marked<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.trim()
        .strip_prefix(MARKER)?
        .trim_start()
        .strip_prefix(key)
}

/// Enough of an unexpected answer to diagnose it, without pasting a whole
/// distribution's MOTD into an error message.
fn truncate(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() <= 200 {
        return s.to_string();
    }
    s.chars().take(200).collect::<String>() + "…"
}

// ---------------------------------------------------------------------------
// `RemoteOps` over `wsl.exe`.
// ---------------------------------------------------------------------------

/// [`RemoteOps`] against one WSL distribution.
///
/// The SSH implementation gets file operations from SFTP; there is no SFTP here,
/// so every one of them is a small POSIX shell script run inside the
/// distribution. That is not a downgrade: the "remote" filesystem is a local
/// disk one syscall away, and a shell is the only interface that is guaranteed
/// to exist in *every* distribution, including ones whose entire userland is a
/// single BusyBox binary.
pub struct WslRemoteOps {
    distro: String,
}

impl WslRemoteOps {
    pub fn new(distro: impl Into<String>) -> Self {
        Self {
            distro: distro.into(),
        }
    }

    /// Run a shell script inside the distribution, script on stdin, from the
    /// distribution's home directory ([`CD_HOME`]).
    fn sh(&self, script: &str, timeout: Duration) -> Result<ExecOutput, String> {
        let script = format!("{CD_HOME}{script}");
        self.invoke(&["sh", "-s"], Some(script.into_bytes()), false, timeout)
    }

    /// A script whose only interesting output is whether it worked.
    fn sh_ok(&self, script: &str) -> Result<(), String> {
        let out = self.sh(script, COMMAND_TIMEOUT)?;
        if out.success() {
            Ok(())
        } else {
            Err(out.failure_reason())
        }
    }

    /// The one place a process is spawned. Everything above this is strings.
    fn invoke(
        &self,
        argv: &[&str],
        input: Option<Vec<u8>>,
        discard_stdout: bool,
        timeout: Duration,
    ) -> Result<ExecOutput, String> {
        let args = wsl_args(&self.distro, argv);
        // The daemon's runtime, the same one `ssh_ops` crosses into. These calls
        // arrive on plain connection threads, never on a runtime worker.
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
        // A timed-out `wsl.exe` must not outlive the future that gave up on it.
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
            // The close is the message: `sh -s` runs nothing until its script
            // ends, and `tee` writes nothing until its input does.
            stdin.shutdown().await?;
        }
        Ok::<(), io::Error>(())
    };

    // Feeding and draining concurrently, because doing them in sequence
    // deadlocks the moment either pipe's buffer fills — and the binary upload is
    // several megabytes.
    let both = async { tokio::join!(feed, child.wait_with_output()) };
    let (written, waited) = tokio::time::timeout(timeout, both)
        .await
        .map_err(|_| format!("the distribution did not answer within {timeout:?}"))?;

    // A broken pipe here usually means the child died first, in which case its
    // own exit status and stderr are the better diagnosis — so this is only
    // reported when nothing else went wrong.
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
        // The status is ignored for the same reason it is over SSH: the script
        // reports on the backgrounding, never on the daemon. Whether the daemon
        // came up is settled by probing its socket.
        self.sh(cmd, LAUNCH_TIMEOUT).map(|_| ())
    }

    /// WSL is the transport that needs one. See [`launch_settle`].
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
        // `-p`, so an existing directory is success rather than the
        // server-specific error SFTP forces the SSH path to disambiguate.
        self.sh_ok(&format!("mkdir -p {}\n", shell_quote(path)))
    }

    fn chmod(&self, path: &str, mode: u32) -> Result<(), String> {
        self.sh_ok(&format!("chmod {mode:o} {}\n", shell_quote(path)))
    }

    /// Write `bytes` to `path` inside the distribution.
    ///
    /// **Through `wsl.exe -- tee <path>`, not through a `\\wsl$\<distro>\…` UNC
    /// path**, and this is the one design decision in this file that had a real
    /// alternative. The UNC path is the worse of the two:
    ///
    /// | | `\\wsl$` UNC write | `tee` over stdio |
    /// |---|---|---|
    /// | Needs to know the distro's user | Yes — the path embeds `\home\<user>` | No — `$HOME` is asked for, and `/etc/wsl.conf` can make it anything |
    /// | Executable bit | Set by the 9p server's defaults, or by a later `chmod` that may not stick without the `metadata` mount option | A real `chmod` inside Linux, on a real filesystem |
    /// | Share name | `\\wsl$` on older builds, `\\wsl.localhost` on newer; both work on some, one on others | Not involved |
    /// | Throughput | 9p is famously slow for Windows→Linux writes | A pipe |
    /// | Compiles/works off Windows | No | Yes (fails cleanly at spawn) |
    ///
    /// The UNC path's only advantage is that it needs no process inside the
    /// distribution — which is irrelevant, because [`RemoteOps`] has to run
    /// commands there anyway.
    ///
    /// `tee` rather than `sh -c 'cat > …'` keeps the path a single argv element
    /// instead of a shell word, and `tee` is present in both coreutils and
    /// BusyBox. Its stdout — a second full copy of the binary — is discarded at
    /// the Windows end rather than redirected inside, which is what lets this
    /// stay shell-free.
    fn put(&self, path: &str, bytes: &[u8]) -> Result<(), String> {
        let out = self.invoke(&["tee", path], Some(bytes.to_vec()), true, PUT_TIMEOUT)?;
        if !out.success() {
            return Err(out.failure_reason());
        }
        // Verify the length. This is not paranoia about `tee`: it is the check
        // that catches any newline translation on the way through `wsl.exe`,
        // which is the one way these bytes could plausibly be altered, and which
        // would otherwise publish a corrupt binary that fails at exec with
        // nothing pointing back here.
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

// ---------------------------------------------------------------------------
// The bundled binary (WSL does not download).
// ---------------------------------------------------------------------------

/// Overrides where the bundled Linux server binaries are looked for. Exists so
/// this can be tested, and so a developer running an unpackaged build can point
/// at a `cargo build --target x86_64-unknown-linux-musl` output.
pub const BUNDLED_DIR_ENV: &str = "TTY7_BUNDLED_SERVER_DIR";

/// The subdirectory beside the client executable that a packaged build puts the
/// Linux server binaries in. **This is the contract with the release workflow**:
/// the Windows installer must place
/// `<install dir>/server/tty7-server-x86_64-unknown-linux-musl` (and the
/// `aarch64` one, for WSL on ARM Windows).
pub const BUNDLED_SUBDIR: &str = "server";

/// Directories a bundled Linux server binary is looked for in, most specific
/// first.
///
/// Pure, and takes both inputs, so the search order is testable without an
/// installed build.
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
        // The packaged layout.
        push(exe_dir.join(BUNDLED_SUBDIR));
        // A flat layout, and where `cargo build` output lands.
        push(exe_dir.to_path_buf());
        // A macOS `.app`, so a developer on a Mac can exercise the discovery
        // half of this without a Windows box.
        if let Some(parent) = exe_dir.parent() {
            push(parent.join("Resources").join(BUNDLED_SUBDIR));
        }
    }
    dirs
}

/// The Linux `tty7-server` this client shipped with.
///
/// A WSL install never downloads: the client's own bundled Linux binary is
/// copied across instead.
/// The version question answers itself, because the bundled binary was built
/// from the same workspace version as the client asking for it; there is no tag
/// to resolve and no manifest to verify against.
pub struct BundledServerBinary {
    dirs: Vec<PathBuf>,
}

impl BundledServerBinary {
    /// Look beside this executable, and at [`BUNDLED_DIR_ENV`] if it is set.
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

    /// Only the directory [`BUNDLED_DIR_ENV`] names, or `None` if it names
    /// nothing.
    ///
    /// [`Self::discover`] also searches beside the executable, which is right
    /// for WSL — the Windows installer puts a Linux binary there on purpose —
    /// and wrong for anything else: a macOS client has its *own* `tty7-server`
    /// next to it, and shipping that to a Linux box would install a binary that
    /// cannot run. So the "install from a local copy" path for real remotes
    /// takes the explicit directory and nothing else.
    pub fn from_env_only() -> Option<Self> {
        let dir = std::env::var_os(BUNDLED_DIR_ENV).filter(|v| !v.is_empty())?;
        Some(Self::in_dirs(vec![PathBuf::from(dir)]))
    }

    /// The first directory that holds `asset`.
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
            // A zero-byte file is a broken packaging step, not a binary. Copying
            // it would produce a "cannot execute" failure inside the
            // distribution with nothing pointing back at the installer.
            return Err(missing(Some(format!("{} (empty file)", path.display()))));
        }
        Ok(LoadedBinary {
            bytes,
            origin: path.display().to_string(),
        })
    }
}

// ---------------------------------------------------------------------------
// The entry point the router calls.
// ---------------------------------------------------------------------------

/// One lock per distribution, held across the whole of [`Installer::run`].
///
/// **Not an optimisation — a correctness requirement.** Restoring a workspace
/// opens one routed connection per pane, all at once, on separate daemon
/// threads. Without this they run the installer concurrently against the same
/// distribution, and the interleaving is destructive rather than merely wasteful:
/// two runs in *this* process share a pid and so share
/// `.tty7-server-c<c>p<p>.<pid>.tmp`, the first renames it into place
/// and reports success, and the second's rename then fails — which sends it down
/// [`Installer::install`]'s recovery branch, whose `remove_file(&paths.binary)`
/// **deletes the binary the first run just published**. Every later pane then
/// execs a path that is no longer there.
///
/// That recovery branch is correct for SSH, where `SshManager`'s per-key
/// `ConnSlot` already serialises connects. WSL has no connection object and so
/// had nothing playing that role; this is it.
///
/// A `std::sync::Mutex` because every caller is a blocking thread, and poisoning
/// is absorbed: a panicked installer leaves no state behind that the next run
/// cannot simply redo.
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

/// Make sure `distro` has this client's `tty7-server` installed and a daemon
/// serving, and return the absolute path of that binary inside the distribution.
///
/// **The WSL counterpart of [`ensure_remote_server`](super::ensure_remote_server).**
/// That one takes an `&Arc<SshConnection>` because everything it does needs SFTP
/// and session channels; there is no connection here to take, and the two share
/// everything that matters anyway — the version-carrying install path, the
/// temp-then-`chmod`-then-`rename` ordering, the `--stdio --bridge` exit-status
/// probe, the launch-and-poll loop, the mismatch record. All of that is
/// [`Installer`], reached through [`Installer::with_source`]; the only
/// differences are which [`RemoteOps`] it drives and where the bytes come from.
///
/// Unlike the SSH entry point this returns the path rather than `()`, because
/// the WSL transport has no `PATH` to fall back on: the link is
/// `wsl.exe -d <distro> -- <this exact path> --stdio`.
///
/// # Call this from the GUI first, on a distribution's first connect
///
/// The router calls this too, but the router runs in the **local daemon
/// process** — and [`set_install_confirm`](super::set_install_confirm) is
/// registered in the **GUI process**. A consent prompt raised from the daemon
/// therefore reaches [`DenyInstall`](super::DenyInstall) and the first install
/// on a distribution fails with "was not confirmed". The same is true of
/// [`take_mismatched_remote_daemons`](super::take_mismatched_remote_daemons),
/// whose registry is a process-local static.
///
/// WSL is the one transport where that is trivially fixable, because there is
/// no connection to own: [`WslRemoteOps`] is just `wsl.exe` invocations, and the
/// GUI is on the *same machine* as the distribution. So the GUI should call this
/// itself before writing a [`RouteHeader`](super::super::router::RouteHeader) —
/// the prompt is then raised where it can be answered, and the daemon's own call
/// a moment later finds the binary installed and asks nobody.
///
/// # No result is cached
///
/// It would be easy to remember the resolved path and skip the rest, and it
/// would be wrong. `wsl --shutdown` is a routine thing for a user to run, and it
/// kills the daemon inside the distribution without touching the binary — so a
/// cached path would keep answering while step 6 no longer held. Every later
/// pane would launch `<path> --stdio`, which with neither `--serve` nor
/// `--bridge` **serves in-process**, and each pane would silently get its own
/// isolated server: sessions stop being shared, panes stop surviving their
/// window, and nothing anywhere reports an error.
///
/// So every connect re-runs the whole check. That is what
/// [`ensure_remote_server`](super::ensure_remote_server) promises too ("it must
/// be safe to call before every link"), and on the common path it costs a
/// `uname`, a stat and the bridge probe against a distribution that is already
/// running.
pub fn ensure_wsl_server(distro: &str) -> io::Result<String> {
    validate_distro(distro)?;
    // Held across the whole run: see `INSTALL_LOCKS`.
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

/// Restart `distro`'s daemon at this client's build — the "restart the service"
/// answer to the version-mismatch prompt, dropping every pane it hosts.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::install::{InstallConfirm, InstallDecision, InstallRequest};
    use std::collections::BTreeMap;
    use std::sync::Mutex as StdMutex;

    // -- names --------------------------------------------------------------

    /// Real distribution names, including the awkward ones people actually have.
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

    /// The refusals. A leading `-` is the one that matters: `wsl.exe` would read
    /// it as an option, and `--` protects the *command*, not the `-d` argument.
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

    /// The WSL command line, spelled out. This is the exact argv the
    /// transport is specified to produce, and it has never been run — so the
    /// string is pinned here instead.
    #[test]
    fn the_transport_command_line_is_the_designs() {
        assert_eq!(
            wsl_args("Ubuntu", &["tty7-server", "--stdio"]),
            vec!["-d", "Ubuntu", "--", "tty7-server", "--stdio"]
        );
        // With a resolved absolute path, which is what actually ships.
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
        // `--` is present even with no command, so a later argv cannot slip in
        // front of it.
        assert_eq!(wsl_args("Ubuntu", &[]), vec!["-d", "Ubuntu", "--"]);

        // `--cd` is deliberately absent: the inbox `wsl.exe` on Windows 10
        // rejects it outright, which would break every invocation rather than
        // just the working directory. See `wsl_args`.
        assert!(
            !wsl_args("Ubuntu", &["sh", "-s"])
                .iter()
                .any(|a| a == "--cd")
        );

        // The distro name is `-d`'s argument and sits *before* `--`, which is
        // why it has to be validated rather than protected by the separator.
        let args = wsl_args("Ubuntu", &["sh", "-s"]);
        assert!(
            args.iter().position(|a| a == "Ubuntu").unwrap()
                < args.iter().position(|a| a == "--").unwrap()
        );
    }

    /// The label a WSL host is prompted about, recorded under, and keyed by has
    /// to be the one `RemoteTarget` already defined — these are
    /// compared as strings in the mismatch registry and in log lines a user is
    /// meant to correlate.
    #[test]
    fn the_host_label_is_the_connection_key() {
        for distro in ["Ubuntu", "Ubuntu-22.04", "my dev box"] {
            let target = crate::core::session::RemoteTarget::Wsl {
                distro: distro.to_string(),
            };
            assert_eq!(host_label(distro), target.connection_key());
        }
    }

    // -- decoding -----------------------------------------------------------

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

    /// `wsl.exe` speaks UTF-16LE; the shell inside the distribution speaks
    /// UTF-8. Both come back on the same pipes, so both have to decode.
    #[test]
    fn both_encodings_on_the_pipe_decode() {
        assert_eq!(decode_wsl_text(&utf16le("Ubuntu\r\n", true)), "Ubuntu\r\n");
        assert_eq!(decode_wsl_text(&utf16le("Ubuntu\r\n", false)), "Ubuntu\r\n");
        assert_eq!(decode_wsl_text(b"/home/me\n"), "/home/me\n");
        assert_eq!(decode_wsl_text(b""), "");
        // Non-ASCII through both paths.
        assert_eq!(decode_wsl_text("héllo".as_bytes()), "héllo");
        assert_eq!(decode_wsl_text(&utf16le("héllo", false)), "héllo");
        // A UTF-8 BOM from a chatty distribution is stripped, not shown.
        assert_eq!(decode_wsl_text("\u{feff}ok".as_bytes()), "ok");
    }

    // -- the scripts, against a real POSIX shell -----------------------------
    //
    // `wsl.exe` cannot be run here, but the *scripts it would carry* are plain
    // POSIX and there is a POSIX shell on this machine. Running them through
    // `sh -s` on stdin — the exact invocation shape the WSL path uses — moves
    // everything except the launcher itself from "never executed" to "executed
    // on every CI platform", and it is the same `/bin/sh` contract a
    // distribution offers.

    /// Feed `script` to a real `sh -s` on stdin and return `(stdout, success)`.
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

    /// A stand-in for the remote `tty7-server`: a script that answers the
    /// settle's probe (`code` 0) or never answers it (anything else).
    #[cfg(unix)]
    fn fake_server(dir: &std::path::Path, name: &str, code: u8) -> String {
        use std::os::unix::fs::PermissionsExt as _;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\nexit {code}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.to_str().unwrap().to_string()
    }

    /// The settle asks the daemon with *the daemon's own binary*, quoted, in the
    /// control dialect — the same question
    /// [`Installer::ensure_daemon`] asks from this side. Not `--pane` (that is
    /// the other socket and would answer for the wrong thing) and not a test on
    /// the socket file, which a stale one passes.
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

    /// **The settle, executed against a daemon that answers.** It stops on the
    /// first answer and still holds the invocation for [`SETTLE_GRACE`] — so the
    /// normal launch is *shorter* than the flat second this replaced, and the
    /// wait is still really there.
    ///
    /// This is the assertion with teeth. A settle that silently became a no-op —
    /// a `sleep` a distro rejects, a line lost in a `format!` — reads as a
    /// passing test everywhere except against a real distribution, which is
    /// where it already cost an afternoon.
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

    /// **The settle, executed against a daemon that never answers.** It spends
    /// its whole budget and then returns — a launch that failed has to become
    /// `ensure_daemon`'s verdict, not a hung invocation.
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

    /// The real budget stays well inside the one its caller allows the *whole*
    /// launch, or a slow daemon turns into a killed invocation instead of a
    /// launched one. Also the guard that both steps are still numbers a `sleep`
    /// accepts.
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

    /// **The stat probe, executed.** Against a real directory, a real
    /// executable, a real non-executable file and a real absent path — the four
    /// answers [`Installer::run`] branches on.
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
        // The size really came back — and `wc -c < f` pads its output with
        // spaces on BSD and not on GNU, which is why the parser splits on
        // whitespace rather than slicing.
        assert_eq!(stat.size, 10);
        assert_eq!(stat.mode & 0o100, 0, "not executable yet");

        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (out, ok) = real_sh(&stat_script(plain.to_str().unwrap()));
        assert!(ok, "{out}");
        assert_ne!(parse_stat(&out).unwrap().unwrap().mode & 0o100, 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **The listing probe, executed**, including a filename with a space and
    /// the empty-vs-absent distinction the first-install prompt rests on.
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

    /// A path with a space and an apostrophe survives the quoting, executed
    /// rather than asserted on the string. This is the case that would silently
    /// produce a *different* answer (about the wrong path) rather than an error.
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

    /// The home probe, executed.
    #[cfg(unix)]
    #[test]
    fn the_home_script_really_reports_an_absolute_path() {
        let (out, ok) = real_sh(HOME_SCRIPT);
        assert!(ok, "{out}");
        let home = out.lines().find_map(|l| marked(l, "home=")).expect("home");
        assert!(home.starts_with('/'), "{home:?}");
    }

    /// **A command inside the script does not eat the rest of the script.**
    ///
    /// This is the hazard `sh -s` introduces and the reason every command the
    /// installer sends redirects its own stdin: the shell is reading its program
    /// from the same pipe, so a child left attached to it would consume the
    /// remaining lines. `daemon_is_serving`'s `< /dev/null` is exactly that
    /// guard, and here it is proven rather than assumed.
    #[cfg(unix)]
    #[test]
    fn a_redirected_command_does_not_consume_the_script_behind_it() {
        // `cat` would swallow everything after it without the redirect.
        let script = format!("cat < /dev/null\nprintf '{MARKER} stat=file 755 42\\n'\n");
        let (out, ok) = real_sh(&script);
        assert!(ok, "{out}");
        assert_eq!(
            parse_stat(&out).unwrap().unwrap().size,
            42,
            "the line after a stdin-reading command must still run"
        );

        // And the real shape: the daemon probe the installer sends, with a
        // stand-in for the server binary that reports "nothing is serving".
        let probe = format!("{} --stdio --bridge < /dev/null\n", shell_quote("false"));
        let (_, ok) = real_sh(&probe);
        assert!(!ok, "a non-serving machine must exit non-zero");
        let probe = format!("{} --stdio --bridge < /dev/null\n", shell_quote("true"));
        let (_, ok) = real_sh(&probe);
        assert!(ok, "a serving machine must exit zero");
    }

    /// **The two encodings really do share a pipe**, and the buffer has to
    /// survive it line by line. `wsl.exe` prepends its own UTF-16 warnings to
    /// stderr in front of whatever the distribution's shell then writes in
    /// UTF-8; a single verdict for the whole buffer would turn one half into
    /// mojibake — and on stderr that half is the reason the user is shown.
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

        // The order the other way round, and with a marker line that a parser
        // downstream still has to find.
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

    /// The four mutating scripts, executed. They are the ones with no output to
    /// parse, so nothing else would notice if they were malformed — the
    /// installer would just report a failed step with the shell's own message.
    #[cfg(unix)]
    #[test]
    fn the_mutating_scripts_really_run() {
        let dir = sh_scratch("mutate");
        let nested = dir.join("a b").join("c'\''d");
        let q = |p: &Path| p.to_str().unwrap().to_string();

        // mkdir -p, through a chain with a space and an apostrophe in it.
        let (_, ok) = real_sh(&format!("mkdir -p {}\n", shell_quote(&q(&nested))));
        assert!(ok);
        assert!(nested.is_dir(), "mkdir did not create {nested:?}");
        // …and again, because the installer walks the chain and re-creates
        // levels that already exist.
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
        // `rm -f` on something absent is success, which the rename-recovery
        // path relies on.
        let (_, ok) = real_sh(&format!("rm -f {}\n", shell_quote(&q(&to))));
        assert!(ok, "rm -f must not fail on a missing file");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every script is prefixed with a `cd` to the home directory, because
    /// `wsl.exe` would otherwise start it in the translated Windows working
    /// directory and the launched daemon would hold a drvfs handle for its whole
    /// life. Executed, including the fallback for a user with no home.
    #[cfg(unix)]
    #[test]
    fn scripts_run_from_the_home_directory() {
        let (out, ok) = real_sh(&format!("{CD_HOME}pwd\n"));
        assert!(ok, "{out}");
        let home = std::env::var("HOME").unwrap();
        assert_eq!(out.trim(), home.trim_end_matches('/'), "cd did not land");

        // A distribution whose default user has no home must still run the
        // script rather than abort it.
        let (out, ok) = real_sh(&format!("HOME=/no/such/place\n{CD_HOME}pwd\n"));
        assert!(ok, "{out}");
        assert_eq!(out.trim(), "/", "the fallback must be `/`, not a failure");
    }

    /// The distribution list, as `wsl.exe -l -q` really emits it: UTF-16LE, CRLF,
    /// a BOM, and Docker Desktop's internal distributions mixed in.
    #[test]
    fn the_distro_list_is_decoded_and_filtered() {
        let raw = utf16le(
            "Ubuntu-22.04\r\ndocker-desktop\r\ndocker-desktop-data\r\nArch\r\n\r\n",
            true,
        );
        assert_eq!(parse_distro_list(&raw), vec!["Ubuntu-22.04", "Arch"]);
        assert!(parse_distro_list(&[]).is_empty());
        // WSL on some builds pads names with NULs rather than emitting clean
        // lines; the trim covers it.
        let padded = utf16le("Ubuntu\0\r\n", true);
        assert_eq!(parse_distro_list(&padded), vec!["Ubuntu"]);
    }

    // -- scripts ------------------------------------------------------------

    /// Paths are single-quoted into the scripts, so a home directory with a
    /// space or an apostrophe cannot end the quoting early. The scripts
    /// themselves never reach a Windows command line — they go in on stdin —
    /// which is what makes this the only quoting layer involved.
    #[test]
    fn script_paths_are_shell_quoted() {
        let script = stat_script("/home/o'brien/my dir/tty7-server-1.0.0");
        assert!(
            script.contains(r"p='/home/o'\''brien/my dir/tty7-server-1.0.0'"),
            "{script}"
        );
        let script = list_script("/home/a b/bin");
        assert!(script.contains("d='/home/a b/bin'"), "{script}");
        // No double quotes around anything we interpolate: the only `"` in the
        // scripts are the fixed `"$p"` / `"$d"` expansions.
        assert!(!list_script("/x").contains("\"/x\""));
    }

    /// The three facts the installer decides on, read back out of the probe.
    #[test]
    fn the_stat_probe_round_trips_its_three_answers() {
        assert_eq!(parse_stat("__tty7_wsl__ stat=none 0 0\n").unwrap(), None);

        let dir = parse_stat("__tty7_wsl__ stat=dir 0 0\n").unwrap().unwrap();
        assert!(dir.is_dir);

        // Executable: the installer treats this as a usable binary.
        let exe = parse_stat("__tty7_wsl__ stat=file 755 6291456\n")
            .unwrap()
            .unwrap();
        assert!(!exe.is_dir);
        assert_eq!(exe.size, 6_291_456);
        assert_ne!(exe.mode & 0o100, 0, "must read as executable");

        // Not executable: a half-finished install, which must be redone rather
        // than exec'd.
        let half = parse_stat("__tty7_wsl__ stat=file 644 6291456\n")
            .unwrap()
            .unwrap();
        assert_eq!(half.mode & 0o100, 0, "must read as not executable");
    }

    /// A distribution that printed something of its own before answering is
    /// still parsed; one that never answered is an error rather than a
    /// confident "not there", because "not there" would trigger a reinstall on
    /// every connect.
    #[test]
    fn the_stat_probe_ignores_noise_but_not_silence() {
        let noisy = "Welcome to Ubuntu 22.04\n__tty7_wsl__ stat=file 755 10\nlast login: …\n";
        assert_eq!(parse_stat(noisy).unwrap().unwrap().size, 10);
        assert!(parse_stat("").is_err());
        assert!(parse_stat("bash: sh: command not found\n").is_err());
        assert!(parse_stat("__tty7_wsl__ stat=weird 0 0\n").is_err());
    }

    /// "Empty" and "absent" are different answers — the first-install prompt
    /// treats both as "never written here", but a directory listing that
    /// silently became `None` on a real directory would suppress the mismatch
    /// bookkeeping too.
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
        // A filename with a space survives, because `read -r` and the `%s`
        // format keep it on one line.
        assert_eq!(
            parse_list("__tty7_wsl__ listing\n__tty7_wsl__ entry=a b c\n").unwrap(),
            Some(vec!["a b c".to_string()])
        );
        assert!(parse_list("nothing at all\n").is_err());
    }

    // -- the bundled binary -------------------------------------------------

    #[test]
    fn the_search_order_is_specific_first_and_deduplicated() {
        let exe = PathBuf::from("/opt/tty7/bin/tty7.exe");
        let dirs = bundled_search_dirs(Some(&exe), None);
        assert_eq!(dirs[0], PathBuf::from("/opt/tty7/bin/server"));
        assert_eq!(dirs[1], PathBuf::from("/opt/tty7/bin"));
        assert!(dirs.contains(&PathBuf::from("/opt/tty7/Resources/server")));

        let over = PathBuf::from("/tmp/mine");
        let dirs = bundled_search_dirs(Some(&exe), Some(&over));
        assert_eq!(dirs[0], over, "the override outranks everything");

        // The override being one of the derived directories must not produce a
        // duplicate probe.
        let same = PathBuf::from("/opt/tty7/bin/server");
        let dirs = bundled_search_dirs(Some(&exe), Some(&same));
        assert_eq!(dirs.iter().filter(|d| **d == same).count(), 1);

        // No executable path at all (a test binary, an embedded context): the
        // override still works, and nothing else is invented.
        assert!(bundled_search_dirs(None, None).is_empty());
        assert_eq!(bundled_search_dirs(None, Some(&over)), vec![over]);
    }

    /// The bundled source loads bytes and reports where they came from, and its
    /// absence is a *named* failure rather than a silent fall back to a
    /// download — which is the whole point of the WSL exception.
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
        assert!(
            msg.contains("tty7-server-x86_64-unknown-linux-musl"),
            "{msg}"
        );
        assert!(msg.contains(&tmp.display().to_string()), "{msg}");
        assert!(matches!(err, InstallError::MissingBundled { .. }));

        // A zero-byte file is a broken packaging step, not an install.
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

    // -- the whole install, without a Windows machine ------------------------

    /// A [`RemoteOps`] that answers like a distribution would, so the WSL
    /// install path can be driven end to end on a Mac. Only the process spawn is
    /// missing; every command string, path and ordering decision below this is
    /// the real one.
    #[derive(Default)]
    struct FakeDistro {
        files: StdMutex<BTreeMap<String, (Vec<u8>, u32)>>,
        dirs: StdMutex<Vec<String>>,
        commands: StdMutex<Vec<String>>,
        /// Set once the daemon has been "launched".
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
                // Every binary this fake holds is one the installer just put
                // there, so it speaks what this build speaks. Anything else
                // cannot answer, exactly like a server older than the flag.
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
            // The `/proc` sweep: no other build is running.
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

    /// **The WSL install, whole.** The bundled binary is copied into the
    /// distribution, published atomically at a versioned path, and a daemon is
    /// launched — with no network, no checksum manifest, and no release tag
    /// anywhere in it.
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

        // The prompt quotes the *local* file it is about to copy, not a URL.
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

        // And the probe that decides "is a daemon already serving" is the exit
        // status of `--stdio --bridge`, not a socket file's existence.
        assert!(
            ops.ran()
                .iter()
                .any(|c| c.contains("--stdio --bridge") && c.contains("/dev/null")),
            "{:?}",
            ops.ran()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Second connect to the same distribution: nothing is written, nothing is
    /// asked, and no daemon is launched. This is the path every pane after the
    /// first takes, so it has to be the cheap one.
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

    /// Declining writes nothing. Same rule as SSH — writing a binary onto a
    /// machine is worth asking about once, and a distribution is still a
    /// filesystem the user owns and did not ask us to touch.
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

    /// A client that shipped without the Linux binary fails with a message that
    /// says so and names the paths — the packaging bug diagnosed at the moment
    /// it bites, rather than as a mysterious connect failure.
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

    /// A distribution running an ARM kernel gets the ARM binary — the asset
    /// selection is the same `uname -sm` mapping the SSH path uses, so WSL on
    /// ARM Windows needs the aarch64 binary bundled too.
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
        // Only the x86_64 asset is bundled, so an ARM distribution must fail
        // rather than be handed a binary that cannot exec.
        let source = bundled(&dir, b"\x7fELF x86");
        let ops = Arm(FakeDistro::default());
        let confirm = Approve(StdMutex::new(Vec::new()));
        let err = Installer::with_source(&ops, &source, &confirm, host_label("Ubuntu"))
            .with_version("26.7.5")
            .run()
            .expect_err("no aarch64 binary bundled");
        let msg = err.to_string();
        assert!(msg.contains("aarch64-unknown-linux-musl"), "{msg}");

        // With it bundled, the same install goes through.
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

    /// The install lock is per distribution: two distros never block each other,
    /// and asking twice for the same one yields the *same* lock (a fresh lock per
    /// call would serialise nothing at all).
    #[test]
    fn the_install_lock_is_shared_per_distro_and_not_across_them() {
        let a1 = install_lock("test-lock-a");
        let a2 = install_lock("test-lock-a");
        let b = install_lock("test-lock-b");
        assert!(Arc::ptr_eq(&a1, &a2), "same distro must share one lock");
        assert!(!Arc::ptr_eq(&a1, &b), "different distros must not block");

        // And it really excludes: while A is held, a second acquire cannot pass.
        let held = a1.lock().unwrap();
        assert!(a2.try_lock().is_err(), "the lock must actually exclude");
        assert!(b.try_lock().is_ok(), "another distro is unaffected");
        drop(held);
        assert!(a2.try_lock().is_ok());
    }

    /// A name that could be misread by `wsl.exe` never reaches a process spawn,
    /// and fails as an argument error rather than as a connect timeout.
    #[test]
    fn ensure_refuses_an_unusable_distro_name_before_spawning_anything() {
        let err = ensure_wsl_server("--shutdown").expect_err("refused");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("leading `-`"), "{err}");

        let err = restart_wsl_daemon("").expect_err("refused");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
}

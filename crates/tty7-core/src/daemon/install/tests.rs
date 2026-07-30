//! The install flow, driven end to end against an in-memory remote.
//!
//! Four things are asked for by name — `uname` parsing, version path
//! construction, atomic replacement, and the sha256 failure path — and none of
//! them may touch the network. The first two are unit-tested in
//! [`super::asset`] and [`super::checksums`]; the last two need the *whole*
//! sequence, which is what the fake remote here provides.
//!
//! The fake keeps a journal of every operation in order. That is what makes
//! "atomic" testable: atomicity is not a property of any single call, it is the
//! claim that the final path is only ever touched by a `rename` of an
//! already-`chmod`ed temp — which is a statement about the *order* of the
//! journal.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use super::*;
use crate::daemon::install::asset::{ASSET_X86_64, CHECKSUMS_ASSET};

const VERSION: &str = "26.7.5";
const HOME: &str = "/home/me";
const BIN_DIR: &str = "/home/me/.local/share/tty7/bin";
const BINARY: &str = "/home/me/.local/share/tty7/bin/tty7-server-26.7.5";
const TEMP: &str = "/home/me/.local/share/tty7/bin/.tty7-server-26.7.5.tmp";

/// Stand-in for the release asset. Content is irrelevant; only its digest is.
const SERVER_BYTES: &[u8] = b"\x7fELF...a static musl tty7-server, pretend it is 6 MB";

// ---------------------------------------------------------------------------
// Fakes.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct FakeFile {
    bytes: Vec<u8>,
    mode: u32,
    is_dir: bool,
}

/// One entry in the journal. Only the operations that can change what is on
/// disk are recorded; reads are not, because no ordering claim depends on them.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Journal {
    Mkdir(String),
    Put { path: String, len: usize },
    Chmod { path: String, mode: u32 },
    Rename { from: String, to: String },
    Remove(String),
    Exec(String),
    Launch,
}

struct FakeRemote {
    files: Mutex<HashMap<String, FakeFile>>,
    journal: Mutex<Vec<Journal>>,
    uname: String,
    /// Set to make every `put` fail, simulating a full disk / read-only home.
    put_error: Option<String>,
    daemon_running: Mutex<bool>,
    /// What `readlink /proc/<pid>/exe` finds, when a daemon is running.
    running_exe: Mutex<Option<String>>,
    /// Whether launching actually starts the fake daemon (false models a binary
    /// that dies on exec).
    launch_works: bool,
    /// What each binary answers to `--protocol`, by path. A path that is absent
    /// models a server too old to know the flag: the probe fails, and the
    /// installer falls back to having no opinion.
    speaks: Mutex<HashMap<String, RemoteProtocol>>,
}

impl FakeRemote {
    fn new() -> Self {
        let mut files = HashMap::new();
        files.insert(
            HOME.to_string(),
            FakeFile {
                bytes: Vec::new(),
                mode: 0o755,
                is_dir: true,
            },
        );
        Self {
            files: Mutex::new(files),
            journal: Mutex::new(Vec::new()),
            uname: "Linux x86_64\n".to_string(),
            put_error: None,
            daemon_running: Mutex::new(false),
            running_exe: Mutex::new(None),
            launch_works: true,
            speaks: Mutex::new(HashMap::new()),
        }
    }

    /// Teach the binary at `exe` to answer `--protocol` with `spoken`.
    fn speaking(self, exe: &str, spoken: RemoteProtocol) -> Self {
        self.speaks.lock().unwrap().insert(exe.to_string(), spoken);
        self
    }

    /// A machine tty7 has installed on before (so consent is not re-asked).
    fn with_previous_install(self, version: &str) -> Self {
        self.preinstall(&format!("{BIN_DIR}/tty7-server-{version}"), 0o755);
        self
    }

    fn preinstall(&self, path: &str, mode: u32) {
        let mut files = self.files.lock().unwrap();
        for dir in asset::remote_paths(HOME, VERSION).dir_chain {
            files.entry(dir).or_insert(FakeFile {
                bytes: Vec::new(),
                mode: 0o700,
                is_dir: true,
            });
        }
        files.insert(
            path.to_string(),
            FakeFile {
                bytes: SERVER_BYTES.to_vec(),
                mode,
                is_dir: false,
            },
        );
    }

    fn serving(self, exe: &str) -> Self {
        *self.daemon_running.lock().unwrap() = true;
        *self.running_exe.lock().unwrap() = Some(exe.to_string());
        self
    }

    fn journal(&self) -> Vec<Journal> {
        self.journal.lock().unwrap().clone()
    }

    fn file(&self, path: &str) -> Option<FakeFile> {
        self.files.lock().unwrap().get(path).cloned()
    }

    fn writes(&self) -> Vec<Journal> {
        self.journal()
            .into_iter()
            .filter(|j| !matches!(j, Journal::Exec(_)))
            .collect()
    }
}

impl RemoteOps for FakeRemote {
    fn home_dir(&self) -> Result<String, String> {
        Ok(HOME.to_string())
    }

    fn run(&self, cmd: &str) -> Result<ExecOutput, String> {
        self.journal.lock().unwrap().push(Journal::Exec(cmd.into()));
        let ok = |stdout: &str| {
            Ok(ExecOutput {
                status: Some(0),
                stdout: stdout.to_string(),
                stderr: String::new(),
            })
        };
        if cmd == "uname -sm" {
            return ok(&self.uname);
        }
        if let Some(exe) = cmd.strip_suffix(&format!(" {PROTOCOL_FLAG}")) {
            let exe = exe.trim_matches('\'');
            return match self.speaks.lock().unwrap().get(exe) {
                Some(spoken) => ok(&serde_json::to_string(spoken).unwrap()),
                // What a server older than the flag does: usage on stderr, and
                // a non-zero status.
                None => Ok(ExecOutput {
                    status: Some(1),
                    stdout: String::new(),
                    stderr: "tty7-server: nothing to do without --daemon or --stdio".into(),
                }),
            };
        }
        if cmd == RUNNING_EXE_COMMAND {
            let exe = self.running_exe.lock().unwrap().clone().unwrap_or_default();
            return ok(&exe);
        }
        if cmd == TERMINATE_RUNNING_COMMAND {
            *self.daemon_running.lock().unwrap() = false;
            *self.running_exe.lock().unwrap() = None;
            return ok("");
        }
        if cmd.contains("--stdio --bridge") {
            let running = *self.daemon_running.lock().unwrap();
            return Ok(ExecOutput {
                status: Some(if running { 0 } else { 1 }),
                stdout: String::new(),
                stderr: if running {
                    String::new()
                } else {
                    "no control server".into()
                },
            });
        }
        if cmd.contains("--daemon") {
            self.journal.lock().unwrap().push(Journal::Launch);
            if self.launch_works {
                *self.daemon_running.lock().unwrap() = true;
                let mut exe = self.running_exe.lock().unwrap();
                if exe.is_none() {
                    *exe = Some(BINARY.to_string());
                }
            }
            return ok("");
        }
        Err(format!("the fake remote does not know `{cmd}`"))
    }

    fn spawn_detached(&self, cmd: &str) -> Result<(), String> {
        self.run(cmd).map(|_| ())
    }

    fn stat(&self, path: &str) -> Result<Option<RemoteStat>, String> {
        Ok(self.file(path).map(|f| RemoteStat {
            size: f.bytes.len() as u64,
            mode: f.mode,
            is_dir: f.is_dir,
        }))
    }

    fn mkdir(&self, path: &str) -> Result<(), String> {
        self.journal
            .lock()
            .unwrap()
            .push(Journal::Mkdir(path.into()));
        self.files
            .lock()
            .unwrap()
            .entry(path.to_string())
            .or_insert(FakeFile {
                bytes: Vec::new(),
                mode: 0o755,
                is_dir: true,
            });
        Ok(())
    }

    fn chmod(&self, path: &str, mode: u32) -> Result<(), String> {
        self.journal.lock().unwrap().push(Journal::Chmod {
            path: path.into(),
            mode,
        });
        match self.files.lock().unwrap().get_mut(path) {
            Some(f) => {
                f.mode = mode;
                Ok(())
            }
            None => Err("2: No such file".into()),
        }
    }

    fn put(&self, path: &str, bytes: &[u8]) -> Result<(), String> {
        self.journal.lock().unwrap().push(Journal::Put {
            path: path.into(),
            len: bytes.len(),
        });
        if let Some(e) = &self.put_error {
            return Err(e.clone());
        }
        self.files.lock().unwrap().insert(
            path.to_string(),
            FakeFile {
                bytes: bytes.to_vec(),
                mode: 0o644,
                is_dir: false,
            },
        );
        Ok(())
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), String> {
        self.journal.lock().unwrap().push(Journal::Rename {
            from: from.into(),
            to: to.into(),
        });
        let mut files = self.files.lock().unwrap();
        match files.remove(from) {
            Some(f) => {
                files.insert(to.to_string(), f);
                Ok(())
            }
            None => Err("2: No such file".into()),
        }
    }

    fn remove_file(&self, path: &str) -> Result<(), String> {
        self.journal
            .lock()
            .unwrap()
            .push(Journal::Remove(path.into()));
        self.files.lock().unwrap().remove(path);
        Ok(())
    }

    fn list_dir(&self, path: &str) -> Result<Option<Vec<String>>, String> {
        let files = self.files.lock().unwrap();
        if !files.get(path).is_some_and(|f| f.is_dir) {
            return Ok(None);
        }
        let prefix = format!("{path}/");
        Ok(Some(
            files
                .keys()
                .filter_map(|k| k.strip_prefix(&prefix))
                .filter(|rest| !rest.contains('/'))
                .map(str::to_string)
                .collect(),
        ))
    }
}

/// Serves a canned release: the asset plus a manifest that really does contain
/// its digest, unless [`FakeRelease::corrupt`] says otherwise.
struct FakeRelease {
    asset_bytes: Vec<u8>,
    /// Bytes the manifest claims the asset hashes to. Differs from
    /// `asset_bytes` in the tampering test.
    manifest_of: Vec<u8>,
    fetched: Mutex<Vec<String>>,
    fail: Option<String>,
}

impl FakeRelease {
    fn new() -> Self {
        Self {
            asset_bytes: SERVER_BYTES.to_vec(),
            manifest_of: SERVER_BYTES.to_vec(),
            fetched: Mutex::new(Vec::new()),
            fail: None,
        }
    }

    /// A release whose manifest does not describe the bytes it serves — a
    /// corrupted download, a rewriting proxy, a tampered mirror.
    fn corrupt(mut self) -> Self {
        self.asset_bytes = b"something else entirely".to_vec();
        self
    }

    fn manifest(&self) -> String {
        format!(
            "{}  {ASSET_X86_64}\n{}  checksums-are-not-self-describing\n",
            checksums::hex(&checksums::sha256(&self.manifest_of)),
            checksums::hex(&checksums::sha256(b"noise")),
        )
    }

    fn fetched(&self) -> Vec<String> {
        self.fetched.lock().unwrap().clone()
    }
}

impl AssetFetcher for FakeRelease {
    fn get(&self, url: &str) -> Result<Vec<u8>, String> {
        self.fetched.lock().unwrap().push(url.to_string());
        if let Some(e) = &self.fail {
            return Err(e.clone());
        }
        if url.ends_with(CHECKSUMS_ASSET) {
            return Ok(self.manifest().into_bytes());
        }
        if url.ends_with(ASSET_X86_64) {
            return Ok(self.asset_bytes.clone());
        }
        Err(format!("404: {url}"))
    }
}

struct FakeUser {
    decision: InstallDecision,
    asked: Mutex<Vec<InstallRequest>>,
}

impl FakeUser {
    fn approving() -> Self {
        Self {
            decision: InstallDecision::Approve,
            asked: Mutex::new(Vec::new()),
        }
    }
    fn declining() -> Self {
        Self {
            decision: InstallDecision::Decline,
            asked: Mutex::new(Vec::new()),
        }
    }
    fn asked(&self) -> Vec<InstallRequest> {
        self.asked.lock().unwrap().clone()
    }
}

impl InstallConfirm for FakeUser {
    fn confirm(&self, request: &InstallRequest) -> InstallDecision {
        self.asked.lock().unwrap().push(request.clone());
        self.decision
    }
}

fn installer<'a>(
    remote: &'a FakeRemote,
    release: &'a FakeRelease,
    user: &'a FakeUser,
    host: &str,
) -> Installer<'a> {
    Installer::new(remote, release, user, host)
        .with_version(VERSION)
        .with_timeouts(Duration::from_millis(200), Duration::from_millis(10))
}

// ---------------------------------------------------------------------------
// The happy path.
// ---------------------------------------------------------------------------

/// All six steps on a machine that has never seen tty7: identify it, find
/// nothing installed, download and verify, ask once, publish atomically, and
/// launch a daemon.
#[test]
fn first_install_runs_all_six_steps() {
    let remote = FakeRemote::new();
    let release = FakeRelease::new();
    let user = FakeUser::approving();

    let report = installer(&remote, &release, &user, "me@fresh-box:22")
        .run()
        .expect("a clean install must succeed");

    assert_eq!(report.asset, ASSET_X86_64);
    assert_eq!(report.paths.binary, BINARY);
    assert!(report.installed, "bytes were transferred");
    assert!(report.confirmed, "a new machine is confirmed once");
    assert!(
        report.launched,
        "nothing was serving, so a daemon was started"
    );
    assert!(report.mismatch.is_none());

    let installed = remote
        .file(BINARY)
        .expect("the binary is at its final path");
    assert_eq!(installed.bytes, SERVER_BYTES, "the verified bytes landed");
    assert_eq!(installed.mode, 0o755, "and are executable");
    assert!(
        remote.file(TEMP).is_none(),
        "the temp name is consumed by the rename"
    );

    // Both release artifacts were fetched from the same tag.
    assert_eq!(
        release.fetched(),
        vec![
            format!("https://github.com/l0ng-ai/tty7/releases/download/v{VERSION}/checksums.txt"),
            format!("https://github.com/l0ng-ai/tty7/releases/download/v{VERSION}/{ASSET_X86_64}"),
        ]
    );
}

/// **Atomic replacement.** The final path must only ever be produced by
/// renaming a temp that is *already* executable — never written to directly,
/// and never chmod'ed after it is visible. Both would leave a window in which a
/// concurrent connect finds `tty7-server-<ver>` present and unusable.
#[test]
fn the_final_path_is_only_ever_reached_by_renaming_a_ready_temp() {
    let remote = FakeRemote::new();
    let release = FakeRelease::new();
    let user = FakeUser::approving();
    installer(&remote, &release, &user, "me@fresh-box:22")
        .run()
        .unwrap();

    let writes = remote.writes();

    // Nothing writes the final path directly.
    assert!(
        !writes
            .iter()
            .any(|j| matches!(j, Journal::Put { path, .. } if path == BINARY)),
        "the binary path is never written to, only renamed onto: {writes:?}"
    );

    let put = writes
        .iter()
        .position(|j| matches!(j, Journal::Put { path, .. } if path == TEMP))
        .expect("the bytes go to the temp path");
    let chmod = writes
        .iter()
        .position(|j| matches!(j, Journal::Chmod { path, mode } if path == TEMP && *mode == 0o755))
        .expect("the temp is made executable");
    let rename = writes
        .iter()
        .position(|j| matches!(j, Journal::Rename { from, to } if from == TEMP && to == BINARY))
        .expect("the temp is renamed onto the binary");

    assert!(put < chmod, "bytes before mode: {writes:?}");
    assert!(
        chmod < rename,
        "the temp is executable before it becomes visible: {writes:?}"
    );
    assert!(
        !writes[rename + 1..]
            .iter()
            .any(|j| matches!(j, Journal::Chmod { path, .. } if path == BINARY)),
        "no chmod after publication — that would be the window this ordering exists to close"
    );
}

/// The directory chain is created outermost-first (SFTP has no `mkdir -p`) and
/// the directory that holds the binaries ends up 0700.
#[test]
fn the_install_directory_is_created_in_order_and_locked_down() {
    let remote = FakeRemote::new();
    let release = FakeRelease::new();
    let user = FakeUser::approving();
    installer(&remote, &release, &user, "me@fresh-box:22")
        .run()
        .unwrap();

    let mkdirs: Vec<String> = remote
        .journal()
        .into_iter()
        .filter_map(|j| match j {
            Journal::Mkdir(p) => Some(p),
            _ => None,
        })
        .collect();
    assert_eq!(
        mkdirs,
        vec![
            "/home/me/.local",
            "/home/me/.local/share",
            "/home/me/.local/share/tty7",
            BIN_DIR,
        ]
    );
    assert_eq!(remote.file(BIN_DIR).unwrap().mode, 0o700);
}

// ---------------------------------------------------------------------------
// sha256 — the failure path.
// ---------------------------------------------------------------------------

/// **A checksum mismatch aborts and writes nothing.** Not a retry, not an
/// unverified install, not a partially-written temp left behind: the remote
/// filesystem must be untouched, and the user must not even have been asked
/// (there is nothing to consent to).
#[test]
fn a_sha256_mismatch_aborts_before_touching_the_remote() {
    let remote = FakeRemote::new();
    let release = FakeRelease::new().corrupt();
    let user = FakeUser::approving();

    let err = installer(&remote, &release, &user, "me@fresh-box:22")
        .run()
        .expect_err("bytes that fail verification must never be installed");

    match err {
        InstallError::Checksum(ChecksumError::Mismatch {
            ref expected,
            ref actual,
            ..
        }) => assert_ne!(expected, actual),
        other => panic!("expected a checksum mismatch, got {other}"),
    }

    assert!(
        remote.writes().is_empty(),
        "nothing may be written after a failed verification: {:?}",
        remote.writes()
    );
    assert!(remote.file(TEMP).is_none());
    assert!(remote.file(BINARY).is_none());
    assert!(
        user.asked().is_empty(),
        "there is nothing to ask about — the download already failed its own check"
    );
    assert_eq!(
        release.fetched().len(),
        2,
        "and it is not retried: one manifest fetch, one asset fetch, then stop"
    );
}

/// A release with no line for our asset is the same class of failure: stop,
/// do not install something unverified.
#[test]
fn a_release_missing_our_asset_aborts() {
    let remote = FakeRemote::new();
    let mut release = FakeRelease::new();
    // Manifest describes a payload nobody serves, under a different name.
    release.manifest_of = b"unrelated".to_vec();
    let user = FakeUser::approving();

    let err = installer(&remote, &release, &user, "me@fresh-box:22")
        .run()
        .unwrap_err();
    assert!(matches!(err, InstallError::Checksum(_)), "got {err}");
    assert!(remote.writes().is_empty());
}

// ---------------------------------------------------------------------------
// Consent.
// ---------------------------------------------------------------------------

/// The prompt has to carry everything it must say: which path, how big,
/// and where the bytes came from.
#[test]
fn the_confirmation_states_path_size_and_origin() {
    let remote = FakeRemote::new();
    let release = FakeRelease::new();
    let user = FakeUser::approving();
    installer(&remote, &release, &user, "me@fresh-box:22")
        .run()
        .unwrap();

    let asked = user.asked();
    assert_eq!(asked.len(), 1, "asked exactly once");
    let request = &asked[0];
    assert_eq!(request.host, "me@fresh-box:22");
    assert_eq!(request.remote_path, BINARY);
    assert_eq!(request.asset, ASSET_X86_64);
    assert_eq!(
        request.size_bytes,
        SERVER_BYTES.len() as u64,
        "the size quoted is the verified byte count, not a Content-Length promise"
    );
    assert!(request.source_url.contains("github.com"));
    assert!(request.source_url.contains(ASSET_X86_64));
    assert_eq!(
        request.sha256,
        checksums::hex(&checksums::sha256(SERVER_BYTES))
    );
    assert_eq!(request.version, VERSION);
}

/// Declining writes nothing and says so. The bytes were already downloaded and
/// verified by then; that is fine, they never left the client.
#[test]
fn declining_installs_nothing() {
    let remote = FakeRemote::new();
    let release = FakeRelease::new();
    let user = FakeUser::declining();

    let err = installer(&remote, &release, &user, "me@fresh-box:22")
        .run()
        .unwrap_err();
    assert!(matches!(err, InstallError::Declined { .. }), "got {err}");
    assert!(
        err.to_string().contains(BINARY),
        "the message names the path"
    );
    assert!(remote.writes().is_empty());
    assert!(remote.file(BINARY).is_none());
}

/// **With no UI attached the default is to refuse, not to proceed.** A daemon
/// running headless must not decide on the user's behalf that writing binaries
/// to their servers is acceptable.
#[test]
fn the_default_confirmation_declines() {
    let request = InstallRequest {
        host: "me@somewhere:22".into(),
        version: VERSION.into(),
        asset: ASSET_X86_64,
        source_url: "https://example/x".into(),
        remote_path: BINARY.into(),
        size_bytes: 42,
        sha256: "00".repeat(32),
    };
    assert_eq!(
        DenyInstall.confirm(&request),
        InstallDecision::Decline,
        "no UI means no consent means no install"
    );
}

/// A machine tty7 has already written to is upgraded silently — the consent was
/// about "may tty7 put binaries here", and it was given.
#[test]
fn upgrading_a_known_machine_does_not_ask_again() {
    let remote = FakeRemote::new().with_previous_install("26.7.4");
    let release = FakeRelease::new();
    let user = FakeUser::declining(); // would refuse if asked

    let report = installer(&remote, &release, &user, "me@known-box:22")
        .run()
        .expect("a silent upgrade must not need consent");

    assert!(report.installed);
    assert!(!report.confirmed);
    assert!(
        user.asked().is_empty(),
        "no prompt on a machine we already use"
    );
    // The older binary is still there: versioned paths coexist.
    assert!(
        remote
            .file(&format!("{BIN_DIR}/tty7-server-26.7.4"))
            .is_some()
    );
    assert!(remote.file(BINARY).is_some());
}

// ---------------------------------------------------------------------------
// Skipping work.
// ---------------------------------------------------------------------------

/// The common path: the right version is already installed and a daemon is
/// serving. No download, no prompt, no write, no launch.
#[test]
fn an_up_to_date_machine_downloads_nothing() {
    let remote = FakeRemote::new();
    remote.preinstall(BINARY, 0o755);
    let remote = remote.serving(BINARY);
    let release = FakeRelease::new();
    let user = FakeUser::declining();

    let report = installer(&remote, &release, &user, "me@current-box:22")
        .run()
        .unwrap();

    assert!(!report.installed);
    assert!(!report.launched);
    assert!(report.mismatch.is_none());
    assert!(release.fetched().is_empty(), "no network at all");
    assert!(remote.writes().is_empty());
}

/// A binary that is present but not executable is a crashed install (the rename
/// landed, the chmod did not). Reinstalling beats launching something the kernel
/// will refuse with `Exec format error`'s equally opaque cousin, `Permission
/// denied`.
#[test]
fn a_present_but_unexecutable_binary_is_reinstalled() {
    let remote = FakeRemote::new();
    remote.preinstall(BINARY, 0o644);
    let release = FakeRelease::new();
    let user = FakeUser::approving();

    let report = installer(&remote, &release, &user, "me@half-installed:22")
        .run()
        .unwrap();

    assert!(report.installed, "a non-executable binary is not usable");
    assert_eq!(remote.file(BINARY).unwrap().mode, 0o755);
}

// ---------------------------------------------------------------------------
// Refusals and write failures.
// ---------------------------------------------------------------------------

/// An architecture we do not publish for is refused before anything is
/// downloaded or written, and the message quotes the machine string verbatim.
#[test]
fn an_unsupported_machine_is_refused_before_any_work() {
    for (uname, expect_linux) in [("Linux armv7l", true), ("Darwin arm64", false)] {
        let mut remote = FakeRemote::new();
        remote.uname = format!("{uname}\n");
        let release = FakeRelease::new();
        let user = FakeUser::approving();

        let err = installer(&remote, &release, &user, "me@odd-box:22")
            .run()
            .unwrap_err();
        match err {
            InstallError::Unsupported(ref target) => {
                assert_eq!(target.raw(), uname);
                assert_eq!(
                    matches!(target, UnsupportedTarget::UnknownMachine { .. }),
                    expect_linux
                );
            }
            other => panic!("{uname} must be refused, got {other}"),
        }
        assert!(err.to_string().contains(uname), "the refusal quotes itself");
        assert!(release.fetched().is_empty(), "nothing downloaded");
        assert!(remote.writes().is_empty(), "nothing written");
    }
}

/// **A failed remote write reports the path and the server's reason, and is not
/// retried anywhere else**. A full disk must not become "let me try
/// /tmp".
#[test]
fn a_failed_write_names_the_path_and_does_not_fall_back() {
    let mut remote = FakeRemote::new();
    remote.put_error = Some("4: Failure (no space left on device)".to_string());
    let release = FakeRelease::new();
    let user = FakeUser::approving();

    let err = installer(&remote, &release, &user, "me@full-disk:22")
        .run()
        .unwrap_err();

    match err {
        InstallError::Write {
            ref path,
            ref reason,
        } => {
            assert_eq!(path, TEMP, "the exact path that failed");
            assert!(reason.contains("no space left"), "the server's own reason");
        }
        other => panic!("expected a write failure, got {other}"),
    }
    let message = err.to_string();
    assert!(message.contains(TEMP), "{message}");
    assert!(message.contains("no space left"), "{message}");

    // One attempt at one path. No second put, no alternative directory.
    let puts: Vec<_> = remote
        .journal()
        .into_iter()
        .filter(|j| matches!(j, Journal::Put { .. }))
        .collect();
    assert_eq!(puts.len(), 1, "not retried: {puts:?}");
    assert!(remote.file(BINARY).is_none());
}

/// A download failure names the URL, so "which release did it even look for" is
/// answerable from the message alone.
#[test]
fn a_download_failure_names_the_url() {
    let remote = FakeRemote::new();
    let mut release = FakeRelease::new();
    release.fail = Some("connection refused".to_string());
    let user = FakeUser::approving();

    let err = installer(&remote, &release, &user, "me@offline:22")
        .run()
        .unwrap_err();
    match err {
        InstallError::Download { ref url, .. } => assert!(url.contains(&format!("v{VERSION}"))),
        other => panic!("expected a download failure, got {other}"),
    }
    assert!(remote.writes().is_empty());
}

// ---------------------------------------------------------------------------
// Step 6: the daemon.
// ---------------------------------------------------------------------------

/// Nothing serving → launch, then confirm by re-probing rather than by trusting
/// the shell's exit status.
#[test]
fn a_daemon_is_launched_when_the_socket_answers_nothing() {
    let remote = FakeRemote::new();
    remote.preinstall(BINARY, 0o755);
    let release = FakeRelease::new();
    let user = FakeUser::declining();

    let report = installer(&remote, &release, &user, "me@idle-box:22")
        .run()
        .unwrap();
    assert!(report.launched);

    let journal = remote.journal();
    let launched = journal.iter().position(|j| *j == Journal::Launch).unwrap();
    assert!(
        journal[..launched]
            .iter()
            .any(|j| matches!(j, Journal::Exec(c) if c.contains("--stdio --bridge"))),
        "the socket is probed before anything is launched"
    );
    assert!(
        journal[launched + 1..]
            .iter()
            .any(|j| matches!(j, Journal::Exec(c) if c.contains("--stdio --bridge"))),
        "and re-probed after, because a shell's exit status says nothing about the daemon"
    );
}

/// A binary that will not stay up fails with a message naming it, rather than
/// leaving the caller to discover it on the first frame.
#[test]
fn a_daemon_that_never_answers_is_an_error() {
    let mut remote = FakeRemote::new();
    remote.launch_works = false;
    remote.preinstall(BINARY, 0o755);
    let release = FakeRelease::new();
    let user = FakeUser::declining();

    let err = installer(&remote, &release, &user, "me@broken-box:22")
        .run()
        .unwrap_err();
    match err {
        InstallError::Launch { ref reason } => assert!(reason.contains(BINARY), "{reason}"),
        other => panic!("expected a launch failure, got {other}"),
    }
}

/// **Version mismatch: keep the old daemon, record the mismatch.** It owns every
/// live pane on that machine; ending them at connect time is the user's call,
/// not the installer's — exactly as `spawn::ensure_running` treats the local
/// daemon.
#[test]
fn an_older_running_daemon_is_kept_and_reported() {
    let remote = FakeRemote::new()
        .with_previous_install("26.7.4")
        .serving(&format!("{BIN_DIR}/tty7-server-26.7.4"));
    let release = FakeRelease::new();
    let user = FakeUser::approving();

    let report = installer(&remote, &release, &user, "me@mismatch-box:22")
        .run()
        .expect("a mismatch is not a failure — the old daemon still works");

    assert!(report.installed, "our version is installed alongside it");
    assert!(!report.launched, "but the running daemon is left alone");
    let mismatch = report.mismatch.expect("the mismatch is reported");
    assert_eq!(mismatch.running_version.as_deref(), Some("26.7.4"));
    assert_eq!(mismatch.wanted_version, VERSION);

    // And it reaches the GUI's take-once queue.
    let queued = take_mismatched_remote_daemons();
    assert!(
        queued.iter().any(|m| m.host == "me@mismatch-box:22"),
        "the keep-or-restart prompt has something to raise: {queued:?}"
    );
}

/// A daemon we cannot identify (no readable `/proc`, a hand-placed binary) is
/// not a mismatch. Having no opinion must never be reported as a disagreement,
/// or every locked-down container would prompt on connect.
#[test]
fn an_unidentifiable_running_daemon_is_not_a_mismatch() {
    let remote = FakeRemote::new();
    remote.preinstall(BINARY, 0o755);
    let remote = remote.serving("");
    let release = FakeRelease::new();
    let user = FakeUser::declining();

    let report = installer(&remote, &release, &user, "me@opaque-box:22")
        .run()
        .unwrap();
    assert!(report.mismatch.is_none());
}

/// Restart is the other branch of the prompt: stop what is running, start ours.
#[test]
fn restart_replaces_the_running_daemon() {
    let remote = FakeRemote::new()
        .with_previous_install("26.7.4")
        .serving(&format!("{BIN_DIR}/tty7-server-26.7.4"));
    remote.preinstall(BINARY, 0o755);
    let release = FakeRelease::new();
    let user = FakeUser::declining();

    installer(&remote, &release, &user, "me@restart-box:22")
        .restart_daemon()
        .expect("restart must succeed");

    let journal = remote.journal();
    let killed = journal
        .iter()
        .position(|j| matches!(j, Journal::Exec(c) if c == TERMINATE_RUNNING_COMMAND))
        .expect("the old daemon is asked to stop");
    let launched = journal.iter().position(|j| *j == Journal::Launch).unwrap();
    assert!(killed < launched, "stop before start — one socket, not two");
    assert!(*remote.daemon_running.lock().unwrap());
}

// ---------------------------------------------------------------------------
// Remote command construction.
// ---------------------------------------------------------------------------

/// The launch detaches the daemon from the SSH session and gives it no stream to
/// hold open. Without either half, closing the channel would kill it (SIGHUP to
/// the session's group) or the channel would never close (inherited stdout).
#[test]
fn the_launch_command_detaches_and_closes_every_stream() {
    let cmd = launch_command("/home/me/.local/share/tty7/bin/tty7-server-26.7.5");
    assert!(cmd.contains("setsid"), "{cmd}");
    assert!(
        cmd.contains("nohup"),
        "a busybox image may have no setsid: {cmd}"
    );
    assert!(cmd.contains("--daemon"), "{cmd}");
    assert!(cmd.contains("< /dev/null"), "{cmd}");
    assert!(cmd.contains("> /dev/null 2>&1"), "{cmd}");
    assert!(
        cmd.trim_end().ends_with("fi"),
        "both branches background it: {cmd}"
    );
}

/// A transport's settle **follows** the launch; it never replaces it. Cheap to
/// get wrong in a `format!` and expensive to notice, because a daemon that was
/// never launched fails exactly like one that died right after being launched.
///
/// And a transport that asks for nothing — every one but WSL — gets the launch
/// line by itself, with no trailing newline to change what the shell reads.
#[test]
fn a_launch_settle_follows_the_launch_and_never_replaces_it() {
    let plain = launch_script(BINARY, None);
    assert_eq!(plain, launch_command(BINARY), "no settle, no wrapping");

    let settled = launch_script(BINARY, Some("sleep 1\n".to_string()));
    assert!(
        settled.starts_with(&plain),
        "the launch survives: {settled}"
    );
    assert!(settled.contains("--daemon"), "{settled}");
    assert!(
        settled.ends_with("sleep 1\n"),
        "the settle is last: {settled}"
    );
}

/// Every command interpolates a remote path, and home directories with spaces
/// or apostrophes exist. Unquoted, `/home/o'brien/...` would end the string
/// mid-path and run whatever followed.
#[test]
fn remote_paths_are_shell_quoted() {
    assert_eq!(shell_quote("/home/me/bin"), "'/home/me/bin'");
    assert_eq!(
        shell_quote("/home/my box/tty7-server"),
        "'/home/my box/tty7-server'"
    );
    assert_eq!(shell_quote("/home/o'brien/x"), r"'/home/o'\''brien/x'");
    // A path that tries to break out stays one argument. The invariant that
    // makes it safe: after the outer quotes, every remaining `'` belongs to a
    // `'\''` escape — so there is no point at which the shell is outside a
    // quoted string and could see `;` as a separator.
    let quoted = shell_quote("/tmp/x'; rm -rf ~; echo '");
    let inner = quoted
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
        .expect("wrapped in single quotes");
    assert!(
        !inner.replace(r"'\''", "\u{0}").contains('\''),
        "every interior quote is escaped, so nothing escapes the quoting: {quoted}"
    );
}

/// The launch command embeds a quoted path, so a hostile-looking home directory
/// cannot turn into a second command.
#[test]
fn the_launch_command_quotes_its_binary() {
    let cmd = launch_command("/home/me/a b/tty7-server-1.0.0");
    assert!(cmd.contains("'/home/me/a b/tty7-server-1.0.0'"), "{cmd}");
}

/// The `/proc` sweep must survive a machine with no tty7-server running (the
/// common case) without the loop's failure becoming the command's — a `set -e`
/// login shell would otherwise report the probe as a broken connection.
#[test]
fn the_running_exe_probe_cannot_fail_the_command() {
    assert!(RUNNING_EXE_COMMAND.trim_end().ends_with("true"));
    assert!(TERMINATE_RUNNING_COMMAND.trim_end().ends_with("true"));
    // It looks only at our own install shape, so it can never terminate
    // something that merely happens to mention tty7.
    assert!(TERMINATE_RUNNING_COMMAND.contains("*/tty7-server-*"));
}

// `connection_label` is now `ConnectionKey::as_str()` verbatim, so what used to
// be tested here — peeling the label out of the derived `Debug` — no longer
// exists. The key's own construction (including the jump chain, which is what
// keeps two hosts behind different bastions from sharing a label) is covered by
// `daemon::ssh::tests`, next to the `base_spec()` helper that builds one.

/// `ExecOutput`'s failure summary prefers what the remote said over a bare
/// number, because "Permission denied" is actionable and "exit status 1" is not.
#[test]
fn exec_failures_quote_stderr_when_there_is_any() {
    let with_stderr = ExecOutput {
        status: Some(127),
        stdout: String::new(),
        stderr: "sh: uname: not found\nmore noise\n".into(),
    };
    assert_eq!(with_stderr.failure_reason(), "sh: uname: not found");

    let silent = ExecOutput {
        status: Some(127),
        stdout: String::new(),
        stderr: "   \n".into(),
    };
    assert_eq!(silent.failure_reason(), "exit status 127");

    let killed = ExecOutput {
        status: None,
        stdout: String::new(),
        stderr: String::new(),
    };
    assert!(killed.failure_reason().contains("killed"));
    assert!(!killed.success());
}

// ---------------------------------------------------------------------------
// `BundledOrRelease` — installing from a local copy instead of a release.
// ---------------------------------------------------------------------------

/// With no bundle configured this is the release download, unchanged. Pinned
/// because it is the path every ordinary user takes, and the whole feature is
/// only acceptable if it is inert until asked for.
#[test]
fn without_a_bundle_the_source_is_the_plain_download() {
    let release = FakeRelease::new();
    let source = BundledOrRelease {
        fetch: &release,
        bundled: None,
    };
    let loaded = source.load("26.7.5", ASSET_X86_64).expect("downloads");
    assert_eq!(loaded.bytes, SERVER_BYTES);
    assert_eq!(
        release.fetched().len(),
        2,
        "the manifest and the asset, i.e. the verified path"
    );
}

/// With one, the bytes come off the disk and **nothing is fetched** — which is
/// the point on an air-gapped client, behind a TLS-intercepting proxy, or on
/// any build with no published release (every developer build).
#[test]
fn a_bundle_is_used_instead_of_downloading() {
    let dir = std::env::temp_dir().join(format!("tty7-bundle-src-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(ASSET_X86_64), b"\x7fELF local build").unwrap();

    let release = FakeRelease::new();
    let source = BundledOrRelease {
        fetch: &release,
        bundled: Some(wsl::BundledServerBinary::in_dirs(vec![dir.clone()])),
    };
    let loaded = source.load("26.7.5", ASSET_X86_64).expect("loads locally");
    assert_eq!(loaded.bytes, b"\x7fELF local build");
    assert!(
        release.fetched().is_empty(),
        "a local install must not touch the network"
    );
    assert!(
        loaded.origin.contains(&dir.display().to_string()),
        "the prompt names where the bytes came from: {}",
        loaded.origin
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A configured directory that lacks *this* asset fails, and does **not**
/// quietly download instead. Someone who pointed at a directory meant to
/// install from it; silently reaching for the network would defeat whichever
/// reason they had — and on an air-gapped box it would fail far from the cause.
#[test]
fn a_bundle_that_lacks_the_asset_does_not_fall_back_to_the_network() {
    let dir = std::env::temp_dir().join(format!("tty7-bundle-empty-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let release = FakeRelease::new();
    let source = BundledOrRelease {
        fetch: &release,
        bundled: Some(wsl::BundledServerBinary::in_dirs(vec![dir.clone()])),
    };
    let err = source.load("26.7.5", ASSET_X86_64).expect_err("no binary");
    assert!(matches!(err, InstallError::MissingBundled { .. }), "{err}");
    assert!(
        err.to_string().contains(&dir.display().to_string()),
        "the error names where it looked: {err}"
    );
    assert!(
        release.fetched().is_empty(),
        "no silent fallback to the network"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The path the installer publishes to is **absolute and version-qualified**,
/// and that is what the session-channel fallback has to exec.
///
/// Observed for real: the transport exec'd the bare name `tty7-server`, which
/// is a `command not found` on a machine where the install had just succeeded —
/// nothing puts `~/.local/share/tty7/bin` on a non-interactive `PATH`, and the
/// file there is not even called `tty7-server`. The remote process died at
/// once, taking the pane with it.
#[test]
fn the_published_path_is_absolute_and_version_qualified() {
    // Built from *this crate's* version rather than the fixture's `VERSION`:
    // what the transport execs is whatever `client_version()` currently names,
    // and pinning the shape to a literal would only re-assert the literal (and
    // go red on every release bump, which is how it used to behave).
    let published = asset::remote_paths(HOME, client_version()).binary;
    assert!(
        published.starts_with('/'),
        "a relative path would resolve against whatever directory the exec landed in"
    );
    assert!(
        published.ends_with(&format!("tty7-server-{}", client_version())),
        "the filename carries the version, so the bare name never names it: {published}"
    );
    assert_ne!(
        published.rsplit('/').next(),
        Some("tty7-server"),
        "if this ever becomes the bare name, `PATH` lookup would start working by accident \
         and the reason for using the absolute path would be forgotten"
    );

    let remote = FakeRemote::new();
    let release = FakeRelease::new();
    let user = FakeUser::approving();
    let report = installer(&remote, &release, &user, "me@fresh-box:22")
        .run()
        .expect("install");
    assert_eq!(
        report.paths.binary, BINARY,
        "this is the string `ensure_remote_server` hands the transport"
    );
}

// ---------------------------------------------------------------------------
// Progress (an 8 MB first install must not look like a hang).
// ---------------------------------------------------------------------------

/// Records every report in order, which is what makes "monotonic" and "reaches
/// the total" testable — neither is a property of any single report.
#[derive(Default)]
struct Reports(Mutex<Vec<(String, InstallPhase)>>);

impl InstallProgress for Reports {
    fn report(&self, host: &str, phase: InstallPhase) {
        self.0.lock().unwrap().push((host.to_string(), phase));
    }
}

impl Reports {
    fn all(&self) -> Vec<(String, InstallPhase)> {
        self.0.lock().unwrap().clone()
    }

    fn phases(&self) -> Vec<InstallPhase> {
        self.all().into_iter().map(|(_, phase)| phase).collect()
    }
}

/// A release whose asset arrives in pieces, like a real HTTP body.
struct ChunkedRelease {
    inner: FakeRelease,
    chunks: usize,
}

impl AssetFetcher for ChunkedRelease {
    fn get(&self, url: &str) -> Result<Vec<u8>, String> {
        self.inner.get(url)
    }

    fn get_with_progress(
        &self,
        url: &str,
        on_progress: &dyn Fn(u64, Option<u64>),
    ) -> Result<Vec<u8>, String> {
        let bytes = self.inner.get(url)?;
        let total = bytes.len() as u64;
        let step = total.div_ceil(self.chunks as u64).max(1);
        let mut done = 0;
        while done < total {
            done = (done + step).min(total);
            on_progress(done, Some(total));
        }
        Ok(bytes)
    }
}

/// **Both halves of the wait are reported, and each one finishes.**
///
/// The download and the upload are separate network hops of the same ~8 MB, and
/// a bar that covered only one of them would sit at 100% through the other —
/// which is the exact failure this exists to prevent.
#[test]
fn an_install_reports_both_transfers_to_completion() {
    let remote = FakeRemote::new();
    let release = ChunkedRelease {
        inner: FakeRelease::new(),
        chunks: 4,
    };
    let user = FakeUser::approving();
    let reports = Arc::new(Reports::default());

    let report = with_install_progress(reports.clone(), || {
        Installer::new(&remote, &release, &user, "me@build-box:22")
            .with_version(VERSION)
            .with_timeouts(Duration::from_millis(200), Duration::from_millis(10))
            .run()
    })
    .expect("install");
    assert!(report.installed, "the fake remote started empty");

    let total = SERVER_BYTES.len() as u64;
    let phases = reports.phases();

    let downloads: Vec<(u64, Option<u64>)> = phases
        .iter()
        .filter_map(|p| match p {
            InstallPhase::Downloading { done, total } => Some((*done, *total)),
            _ => None,
        })
        .collect();
    assert!(
        downloads.len() > 1,
        "a chunked body should report more than once: {downloads:?}"
    );
    assert_eq!(
        downloads.last().map(|(done, _)| *done),
        Some(total),
        "the download's last report is the whole asset"
    );
    assert!(
        downloads.windows(2).all(|w| w[0].0 <= w[1].0),
        "a bar that goes backwards reads as a restart: {downloads:?}"
    );

    let uploads: Vec<u64> = phases
        .iter()
        .filter_map(|p| match p {
            InstallPhase::Uploading { done, .. } => Some(*done),
            _ => None,
        })
        .collect();
    assert_eq!(
        uploads.last(),
        Some(&total),
        "the upload reaches the byte count the consent prompt quoted"
    );

    // Order matters: the client cannot push bytes it has not fetched, and a UI
    // that saw them interleaved would have to decide which one to draw.
    let first_upload = phases
        .iter()
        .position(|p| matches!(p, InstallPhase::Uploading { .. }))
        .expect("an upload");
    let last_download = phases
        .iter()
        .rposition(|p| matches!(p, InstallPhase::Downloading { .. }))
        .expect("a download");
    assert!(
        last_download < first_upload,
        "downloading finishes before uploading starts: {phases:?}"
    );
}

/// **Every report names the machine it is about.**
///
/// The GUI keys its progress slots by machine, so a report that arrived with the
/// wrong label — or an empty one — would paint one box's bytes under another's
/// name while both were installing.
#[test]
fn every_report_carries_the_host() {
    let remote = FakeRemote::new();
    let release = ChunkedRelease {
        inner: FakeRelease::new(),
        chunks: 3,
    };
    let user = FakeUser::approving();
    let reports = Arc::new(Reports::default());

    with_install_progress(reports.clone(), || {
        Installer::new(&remote, &release, &user, "me@build-box:22")
            .with_version(VERSION)
            .with_timeouts(Duration::from_millis(200), Duration::from_millis(10))
            .run()
    })
    .expect("install");

    let hosts: Vec<String> = reports.all().into_iter().map(|(host, _)| host).collect();
    assert!(!hosts.is_empty(), "the install reported something");
    assert!(
        hosts.iter().all(|h| h == "me@build-box:22"),
        "one install, one machine: {hosts:?}"
    );
}

/// **An install that is already present reports nothing.**
///
/// The common path — a machine tty7 has installed to before — does no transfer
/// at all, and a bar that flashed on every connect would train the user to
/// ignore it on the one connect where it means something.
#[test]
fn a_present_binary_reports_no_progress() {
    let remote = FakeRemote::new().with_previous_install(VERSION);
    let release = FakeRelease::new();
    let user = FakeUser::approving();
    let reports = Arc::new(Reports::default());

    let report = with_install_progress(reports.clone(), || {
        installer(&remote, &release, &user, "me@build-box:22").run()
    })
    .expect("install");

    assert!(!report.installed, "nothing was written");
    assert!(
        reports.phases().is_empty(),
        "nothing transferred, so nothing to show: {:?}",
        reports.phases()
    );
}

/// **The scoped sink outranks the global one, and is put back afterwards.**
///
/// Same contract as `with_install_confirm`, and it matters for the same reason:
/// in the daemon each routed connection has its own client, and a global would
/// send one machine's byte counts to the other machine's window.
#[test]
fn a_scoped_progress_sink_outranks_the_global_one() {
    let scoped = Arc::new(Reports::default());
    let phase = InstallPhase::Uploading { done: 1, total: 2 };

    install_progress().report("before", phase);
    with_install_progress(scoped.clone(), || {
        install_progress().report("inside", phase);
    });
    install_progress().report("after", phase);

    let seen: Vec<String> = scoped.all().into_iter().map(|(host, _)| host).collect();
    assert_eq!(
        seen,
        vec!["inside".to_string()],
        "only the reports raised inside the scope land in it"
    );
}

/// **`fraction` is safe to hand straight to a layout.**
///
/// It feeds a width, so anything outside `0.0..=1.0` draws a bar that overflows
/// its track or inverts it. A zero or absent total is the interesting case: it
/// means "unknown", not "zero percent", and the caller has to be able to tell.
#[test]
fn a_fraction_is_either_absent_or_in_range() {
    assert_eq!(
        InstallPhase::Downloading {
            done: 0,
            total: None
        }
        .fraction(),
        None,
        "no Content-Length means no fraction to draw"
    );
    assert_eq!(
        InstallPhase::Uploading { done: 5, total: 0 }.fraction(),
        None,
        "a zero total is unknown, not complete"
    );
    assert_eq!(
        InstallPhase::Uploading {
            done: 50,
            total: 100
        }
        .fraction(),
        Some(0.5)
    );
    assert_eq!(
        InstallPhase::Uploading {
            done: 200,
            total: 100
        }
        .fraction(),
        Some(1.0),
        "an over-count is clamped rather than overflowing the track"
    );
}

// ---------------------------------------------------------------------------
// Dialects, not build strings.
// ---------------------------------------------------------------------------

/// What this client speaks, which is what a remote has to match.
fn ours() -> RemoteProtocol {
    RemoteProtocol {
        build: VERSION.to_string(),
        ..RemoteProtocol::of_this_build()
    }
}

const OTHER_BUILD: &str = "26.7.9-nightly.20260801";
const OTHER_EXE: &str = "/home/me/.local/share/tty7/bin/tty7-server-26.7.9-nightly.20260801";

/// **A newer server this client can talk to is adopted, not overwritten.**
///
/// The scene from the field: a `26.7.6` client meets a machine already serving
/// `26.7.7-nightly`, both speaking the same dialects. Before this, the client
/// stat'ed for its *own* version, missed, uploaded 8 MB nobody needed, and then
/// asked the user to choose between keeping their sessions and restarting a
/// server that was working fine.
#[test]
fn a_compatible_running_server_is_reused_without_installing() {
    let remote = FakeRemote::new().serving(OTHER_EXE).speaking(
        OTHER_EXE,
        RemoteProtocol {
            build: OTHER_BUILD.to_string(),
            ..ours()
        },
    );
    let release = FakeRelease::new();
    let user = FakeUser::approving();

    let report = installer(&remote, &release, &user, "me@build-box:22")
        .run()
        .expect("connect");

    assert!(
        !report.installed,
        "nothing needed writing: {:?}",
        remote.writes()
    );
    assert!(
        report.reused.is_some(),
        "the running server was adopted deliberately, and the report says so"
    );
    assert_eq!(
        report.paths.binary, OTHER_EXE,
        "the transport must connect to the binary that is actually serving"
    );
    assert!(
        report.mismatch.is_none(),
        "same dialects, so there is nothing to ask the user about"
    );
    assert!(
        remote.writes().is_empty(),
        "not one byte written to a machine that needed nothing: {:?}",
        remote.writes()
    );
    assert!(
        release.fetched().is_empty(),
        "and nothing downloaded either: {:?}",
        release.fetched()
    );
}

/// **A server speaking a different dialect is still installed over.**
///
/// The other half of the same judgement — adoption is not a blanket "reuse
/// whatever is there". A control dialect we cannot speak is exactly what the
/// prompt exists for.
#[test]
fn an_incompatible_running_server_is_not_adopted() {
    let remote = FakeRemote::new().serving(OTHER_EXE).speaking(
        OTHER_EXE,
        RemoteProtocol {
            build: OTHER_BUILD.to_string(),
            control: ours().control + 1,
            ..ours()
        },
    );
    let release = FakeRelease::new();
    let user = FakeUser::approving();

    let report = installer(&remote, &release, &user, "me@build-box:22")
        .run()
        .expect("connect");

    assert!(report.installed, "a dialect we cannot speak means install");
    assert!(report.reused.is_none());
    assert_eq!(
        report.paths.binary, BINARY,
        "and the transport uses the one we just installed"
    );
    assert!(
        report.mismatch.is_some(),
        "the user still has a choice to make about the daemon that is running"
    );
}

/// **The pane dialect counts too, not just the control one.**
///
/// A remote workspace uses both: control for the workspace, the pane protocol
/// for every terminal in it. Matching one and not the other would open the
/// workspace and then fail on the first pane.
#[test]
fn a_matching_control_dialect_is_not_enough_on_its_own() {
    let remote = FakeRemote::new().serving(OTHER_EXE).speaking(
        OTHER_EXE,
        RemoteProtocol {
            build: OTHER_BUILD.to_string(),
            protocol: ours().protocol + 1,
            ..ours()
        },
    );
    let release = FakeRelease::new();
    let user = FakeUser::approving();

    let report = installer(&remote, &release, &user, "me@build-box:22")
        .run()
        .expect("connect");

    assert!(
        report.installed,
        "the control versions agreed, but panes would not have worked"
    );
    assert!(report.reused.is_none());
}

/// **A server too old to answer `--protocol` is handled exactly as before.**
///
/// It predates the flag, so it exits non-zero; we learn nothing, and "nothing
/// learnt" has to keep meaning "install ours and let the user decide", never
/// "assume it is fine".
#[test]
fn a_server_that_cannot_be_probed_is_installed_over() {
    // `.serving` without `.speaking`: the probe fails.
    let remote = FakeRemote::new().serving(OTHER_EXE);
    let release = FakeRelease::new();
    let user = FakeUser::approving();

    let report = installer(&remote, &release, &user, "me@build-box:22")
        .run()
        .expect("connect");

    assert!(report.installed, "no answer means no adoption");
    assert!(report.reused.is_none());
    assert!(
        report.mismatch.is_some(),
        "an unprobeable different build is exactly when the prompt is honest"
    );
}

/// **Our own version already installed still short-circuits everything.**
///
/// The fast path must not have grown a probe: a machine we have installed on
/// before should cost a `stat` and nothing more.
#[test]
fn the_matching_version_still_costs_no_probe() {
    let remote = FakeRemote::new()
        .with_previous_install(VERSION)
        .serving(BINARY);
    let release = FakeRelease::new();
    let user = FakeUser::approving();

    let report = installer(&remote, &release, &user, "me@build-box:22")
        .run()
        .expect("connect");

    assert!(!report.installed);
    assert!(report.reused.is_none(), "adoption is for *other* builds");
    assert!(
        !remote
            .journal()
            .iter()
            .any(|j| matches!(j, Journal::Exec(cmd) if cmd.ends_with(PROTOCOL_FLAG))),
        "nothing to ask: the running exe is the path we wanted: {:?}",
        remote.journal()
    );
}

/// **`serves` is symmetric in neither direction by accident — it is equality.**
///
/// Written down because "newer can serve older" is the tempting wrong rule, and
/// the failure it produces (a wire error mid-session, long after the connect)
/// is far worse than the prompt it avoids.
#[test]
fn only_identical_dialects_serve() {
    let base = ours();
    assert!(base.serves(&base));
    assert!(
        base.serves(&RemoteProtocol {
            build: "some other build entirely".to_string(),
            ..base.clone()
        }),
        "the build string decides nothing"
    );
    assert!(
        !base.serves(&RemoteProtocol {
            control: base.control + 1,
            ..base.clone()
        }),
        "a newer client is not automatically served by an older server"
    );
    assert!(
        !RemoteProtocol {
            control: base.control + 1,
            ..base.clone()
        }
        .serves(&base),
        "nor the other way round"
    );
}

/// **The probe's output survives a chatty login shell.**
///
/// `.bashrc` on a shared box prints banners, `direnv` prints exports, and all of
/// it lands on the same stdout the JSON does.
#[test]
fn a_noisy_shell_does_not_break_the_probe() {
    let spoken = ours();
    let json = serde_json::to_string(&spoken).unwrap();

    assert_eq!(RemoteProtocol::parse(&json), Some(spoken.clone()));
    assert_eq!(
        RemoteProtocol::parse(&format!(
            "Welcome to build-box!\nLast login: today\n{json}\n"
        )),
        Some(spoken),
        "the answer is the last line, because the server prints it at exit"
    );
    assert_eq!(RemoteProtocol::parse(""), None);
    assert_eq!(RemoteProtocol::parse("not json at all"), None);
}

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use super::*;
use crate::daemon::install::asset::{ASSET_X86_64, CHECKSUMS_ASSET};

const VERSION: &str = "26.7.5";
const CONTROL: u32 = 3;
const PROTOCOL: u32 = 4;
const HOME: &str = "/home/me";
const BIN_DIR: &str = "/home/me/.local/share/tty7/bin";
const BINARY: &str = "/home/me/.local/share/tty7/bin/tty7-server-c3p4";
const TEMP_BASE: &str = "/home/me/.local/share/tty7/bin/.tty7-server-c3p4.tmp";

fn temp() -> String {
    unique_temp(TEMP_BASE)
}

const SERVER_BYTES: &[u8] = b"\x7fELF...a static musl tty7-server, pretend it is 6 MB";

#[derive(Clone, Debug)]
struct FakeFile {
    bytes: Vec<u8>,
    mode: u32,
    is_dir: bool,
}

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
    put_error: Option<String>,
    daemon_running: Mutex<bool>,
    running_exe: Mutex<Option<String>>,
    launch_works: bool,
    speaks: Mutex<HashMap<String, RemoteProtocol>>,
    installed_speaks: Option<RemoteProtocol>,
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
            installed_speaks: Some(ours()),
        }
    }

    fn speaking(self, exe: &str, spoken: RemoteProtocol) -> Self {
        self.speaks.lock().unwrap().insert(exe.to_string(), spoken);
        self
    }

    fn uploads_speaking(mut self, spoken: Option<RemoteProtocol>) -> Self {
        self.installed_speaks = spoken;
        self
    }

    fn with_previous_install(self) -> Self {
        self.preinstall(BINARY, 0o755);
        self.speaks
            .lock()
            .unwrap()
            .insert(BINARY.to_string(), ours());
        self
    }

    fn with_legacy_install(self, version: &str) -> (Self, String) {
        let path = format!("{BIN_DIR}/tty7-server-{version}");
        self.preinstall(&path, 0o755);
        (self, path)
    }

    fn preinstall(&self, path: &str, mode: u32) {
        let mut files = self.files.lock().unwrap();
        for dir in asset::remote_paths(HOME, CONTROL, PROTOCOL).dir_chain {
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
        let mut speaks = self.speaks.lock().unwrap();
        match &self.installed_speaks {
            Some(spoken) => speaks.insert(path.to_string(), spoken.clone()),
            None => speaks.remove(path),
        };
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
                let mut speaks = self.speaks.lock().unwrap();
                match speaks.remove(from) {
                    Some(spoken) => speaks.insert(to.to_string(), spoken),
                    None => speaks.remove(to),
                };
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

struct FakeRelease {
    asset_bytes: Vec<u8>,
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
    release: &'a dyn AssetFetcher,
    user: &'a FakeUser,
    host: &str,
) -> Installer<'a> {
    Installer::new(remote, release, user, host)
        .with_version(VERSION)
        .with_dialect(CONTROL, PROTOCOL)
        .with_timeouts(Duration::from_millis(200), Duration::from_millis(10))
}

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
        remote.file(&temp()).is_none(),
        "the temp name is consumed by the rename"
    );

    assert_eq!(
        release.fetched(),
        vec![
            format!("https://github.com/l0ng-ai/tty7/releases/download/v{VERSION}/checksums.txt"),
            format!("https://github.com/l0ng-ai/tty7/releases/download/v{VERSION}/{ASSET_X86_64}"),
        ]
    );
}

#[test]
fn the_final_path_is_only_ever_reached_by_renaming_a_ready_temp() {
    let remote = FakeRemote::new();
    let release = FakeRelease::new();
    let user = FakeUser::approving();
    installer(&remote, &release, &user, "me@fresh-box:22")
        .run()
        .unwrap();

    let writes = remote.writes();

    assert!(
        !writes
            .iter()
            .any(|j| matches!(j, Journal::Put { path, .. } if path == BINARY)),
        "the binary path is never written to, only renamed onto: {writes:?}"
    );

    let put = writes
        .iter()
        .position(|j| matches!(j, Journal::Put { path, .. } if path == &temp()))
        .expect("the bytes go to the temp path");
    let chmod = writes
        .iter()
        .position(
            |j| matches!(j, Journal::Chmod { path, mode } if path == &temp() && *mode == 0o755),
        )
        .expect("the temp is made executable");
    let rename = writes
        .iter()
        .position(|j| matches!(j, Journal::Rename { from, to } if from == &temp() && to == BINARY))
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
    assert!(remote.file(&temp()).is_none());
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

#[test]
fn a_release_missing_our_asset_aborts() {
    let remote = FakeRemote::new();
    let mut release = FakeRelease::new();
    release.manifest_of = b"unrelated".to_vec();
    let user = FakeUser::approving();

    let err = installer(&remote, &release, &user, "me@fresh-box:22")
        .run()
        .unwrap_err();
    assert!(matches!(err, InstallError::Checksum(_)), "got {err}");
    assert!(remote.writes().is_empty());
}

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

#[test]
fn upgrading_a_known_machine_does_not_ask_again() {
    let (remote, legacy) = FakeRemote::new().with_legacy_install("26.7.4");
    let release = FakeRelease::new();
    let user = FakeUser::declining();

    let report = installer(&remote, &release, &user, "me@known-box:22")
        .run()
        .expect("a silent upgrade must not need consent");

    assert!(report.installed);
    assert!(!report.confirmed);
    assert!(
        user.asked().is_empty(),
        "no prompt on a machine we already use"
    );
    assert!(remote.file(&legacy).is_some());
    assert!(remote.file(BINARY).is_some());
}

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
            assert_eq!(path, &temp(), "the exact path that failed");
            assert!(reason.contains("no space left"), "the server's own reason");
        }
        other => panic!("expected a write failure, got {other}"),
    }
    let message = err.to_string();
    assert!(message.contains(&temp()), "{message}");
    assert!(message.contains("no space left"), "{message}");

    let puts: Vec<_> = remote
        .journal()
        .into_iter()
        .filter(|j| matches!(j, Journal::Put { .. }))
        .collect();
    assert_eq!(puts.len(), 1, "not retried: {puts:?}");
    assert!(remote.file(BINARY).is_none());
}

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

#[test]
fn an_older_running_daemon_is_kept_and_reported() {
    let (remote, legacy) = FakeRemote::new().with_legacy_install("26.7.4");
    let remote = remote.serving(&legacy).speaking(
        &legacy,
        RemoteProtocol {
            control: CONTROL - 1,
            protocol: PROTOCOL,
            build: "26.7.4".to_string(),
        },
    );
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

    let queued = take_mismatched_remote_daemons();
    assert!(
        queued.iter().any(|m| m.host == "me@mismatch-box:22"),
        "the keep-or-restart prompt has something to raise: {queued:?}"
    );
}

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

#[test]
fn restart_replaces_the_running_daemon() {
    let remote = FakeRemote::new()
        .with_previous_install()
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

#[test]
fn remote_paths_are_shell_quoted() {
    assert_eq!(shell_quote("/home/me/bin"), "'/home/me/bin'");
    assert_eq!(
        shell_quote("/home/my box/tty7-server"),
        "'/home/my box/tty7-server'"
    );
    assert_eq!(shell_quote("/home/o'brien/x"), r"'/home/o'\''brien/x'");
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

#[test]
fn the_launch_command_quotes_its_binary() {
    let cmd = launch_command("/home/me/a b/tty7-server-1.0.0");
    assert!(cmd.contains("'/home/me/a b/tty7-server-1.0.0'"), "{cmd}");
}

#[test]
fn the_running_exe_probe_cannot_fail_the_command() {
    assert!(RUNNING_EXE_COMMAND.trim_end().ends_with("true"));
    assert!(TERMINATE_RUNNING_COMMAND.trim_end().ends_with("true"));
    assert!(TERMINATE_RUNNING_COMMAND.contains("*/tty7-server-*"));
}

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

#[test]
fn without_a_bundle_the_source_is_the_plain_download() {
    let release = FakeRelease::new();
    let source = BundledOrRelease {
        fetch: &release,
        bundled: None,
        fallback_on_missing: false,
    };
    let loaded = source.load("26.7.5", ASSET_X86_64).expect("downloads");
    assert_eq!(loaded.bytes, SERVER_BYTES);
    assert_eq!(
        release.fetched().len(),
        2,
        "the manifest and the asset, i.e. the verified path"
    );
}

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
        fallback_on_missing: false,
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

#[test]
fn a_bundle_that_lacks_the_asset_does_not_fall_back_to_the_network() {
    let dir = std::env::temp_dir().join(format!("tty7-bundle-empty-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let release = FakeRelease::new();
    let source = BundledOrRelease {
        fetch: &release,
        bundled: Some(wsl::BundledServerBinary::in_dirs(vec![dir.clone()])),
        fallback_on_missing: false,
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

#[test]
fn discover_falls_back_to_release_when_bundled_is_missing() {
    let dir = std::env::temp_dir().join(format!("tty7-bundle-discover-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let release = FakeRelease::new();
    let source = BundledOrRelease {
        fetch: &release,
        bundled: Some(wsl::BundledServerBinary::in_dirs(vec![dir.clone()])),
        fallback_on_missing: true,
    };
    let loaded = source
        .load("26.7.5", ASSET_X86_64)
        .expect("falls back to release");
    assert_eq!(loaded.bytes, SERVER_BYTES);
    assert_eq!(
        release.fetched().len(),
        2,
        "the manifest and the asset must be fetched when the bundled binary is absent"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn discover_uses_bundled_when_it_is_present() {
    let dir = std::env::temp_dir().join(format!(
        "tty7-bundle-discover-present-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(ASSET_X86_64), b"\x7fELF discovered build").unwrap();

    let release = FakeRelease::new();
    let source = BundledOrRelease {
        fetch: &release,
        bundled: Some(wsl::BundledServerBinary::in_dirs(vec![dir.clone()])),
        fallback_on_missing: true,
    };
    let loaded = source.load("26.7.5", ASSET_X86_64).expect("loads locally");
    assert_eq!(loaded.bytes, b"\x7fELF discovered build");
    assert!(
        release.fetched().is_empty(),
        "a discovered bundled install must not touch the network"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_published_path_is_absolute_and_dialect_qualified() {
    let real = RemoteProtocol::of_this_build();
    let published = asset::remote_paths(HOME, real.control, real.protocol).binary;
    assert!(
        published.starts_with('/'),
        "a relative path would resolve against whatever directory the exec landed in"
    );
    assert_eq!(
        asset::dialect_from_path(&published),
        Some((real.control, real.protocol)),
        "the filename carries the dialects, so the bare name never names it: {published}"
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
        installer(&remote, &release, &user, "me@build-box:22").run()
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
        installer(&remote, &release, &user, "me@build-box:22").run()
    })
    .expect("install");

    let hosts: Vec<String> = reports.all().into_iter().map(|(host, _)| host).collect();
    assert!(!hosts.is_empty(), "the install reported something");
    assert!(
        hosts.iter().all(|h| h == "me@build-box:22"),
        "one install, one machine: {hosts:?}"
    );
}

#[test]
fn a_present_binary_reports_no_progress() {
    let remote = FakeRemote::new().with_previous_install();
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

fn ours() -> RemoteProtocol {
    RemoteProtocol {
        control: CONTROL,
        protocol: PROTOCOL,
        build: VERSION.to_string(),
    }
}

const OTHER_BUILD: &str = "26.7.9-nightly.20260801";
const OTHER_EXE: &str = "/home/me/.local/share/tty7/bin/tty7-server-26.7.9-nightly.20260801";

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

#[test]
fn a_server_that_cannot_be_probed_is_installed_over() {
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

#[test]
fn the_matching_version_still_costs_no_probe() {
    let remote = FakeRemote::new().with_previous_install().serving(BINARY);
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

#[test]
fn an_upload_that_speaks_the_wrong_dialect_is_not_published() {
    let remote = FakeRemote::new().uploads_speaking(Some(RemoteProtocol {
        control: CONTROL - 1,
        protocol: PROTOCOL,
        build: "26.7.4".to_string(),
    }));
    let release = FakeRelease::new();
    let user = FakeUser::approving();

    let err = installer(&remote, &release, &user, "me@stale-box:22")
        .run()
        .unwrap_err();

    match err {
        InstallError::DialectMismatch { ref spoke, .. } => assert_eq!(
            spoke.as_ref().map(|s| s.dialect()),
            Some((CONTROL - 1, PROTOCOL)),
            "the error quotes what the bytes actually said"
        ),
        other => panic!("expected a dialect mismatch, got {other}"),
    }
    assert!(
        remote.file(BINARY).is_none(),
        "nothing may sit at the published name"
    );
    assert!(
        remote.file(&temp()).is_none(),
        "and the staged file is cleaned up rather than left to be found"
    );
    assert!(
        err.to_string().contains(wsl::BUNDLED_DIR_ENV),
        "the message points at the one lever that fixes it: {err}"
    );
}

#[test]
fn an_upload_that_cannot_answer_is_not_published() {
    let remote = FakeRemote::new().uploads_speaking(None);
    let release = FakeRelease::new();
    let user = FakeUser::approving();

    let err = installer(&remote, &release, &user, "me@wrong-arch:22")
        .run()
        .unwrap_err();

    assert!(
        matches!(err, InstallError::DialectMismatch { spoke: None, .. }),
        "got {err}"
    );
    assert!(remote.file(BINARY).is_none());
}

#[test]
fn a_machine_with_our_dialect_installed_costs_nothing() {
    let remote = FakeRemote::new().with_previous_install().serving(BINARY);
    let release = FakeRelease::new();
    let user = FakeUser::declining();

    let report = installer(&remote, &release, &user, "me@ready-box:22")
        .run()
        .expect("connect");

    assert!(!report.installed);
    assert!(report.mismatch.is_none());
    assert!(remote.writes().is_empty(), "{:?}", remote.writes());
    assert!(release.fetched().is_empty(), "{:?}", release.fetched());
}

#[test]
fn another_build_at_our_dialect_is_used_rather_than_replaced() {
    let remote = FakeRemote::new().with_previous_install().serving(BINARY);
    let remote = remote.speaking(
        BINARY,
        RemoteProtocol {
            build: OTHER_BUILD.to_string(),
            ..ours()
        },
    );
    let release = FakeRelease::new();
    let user = FakeUser::declining();

    let report = installer(&remote, &release, &user, "me@shared-box:22")
        .run()
        .expect("connect");

    assert!(!report.installed, "{:?}", remote.writes());
    assert!(
        report.mismatch.is_none(),
        "a different build at the same dialect is not a question for the user"
    );
}

#[test]
fn a_legacy_named_binary_is_probed_not_assumed() {
    let (remote, legacy) = FakeRemote::new().with_legacy_install(VERSION);
    let remote = remote.serving(&legacy).speaking(
        &legacy,
        RemoteProtocol {
            build: VERSION.to_string(),
            ..ours()
        },
    );
    let release = FakeRelease::new();
    let user = FakeUser::approving();

    let report = installer(&remote, &release, &user, "me@legacy-box:22")
        .run()
        .expect("connect");

    assert!(
        !report.installed,
        "it speaks our dialects, so it serves: {:?}",
        remote.writes()
    );
    assert_eq!(
        report.paths.binary, legacy,
        "and that is what we connect to"
    );
    assert!(report.mismatch.is_none());
}

#[test]
fn the_staging_path_carries_the_pid() {
    let staged = temp();
    assert_ne!(staged, TEMP_BASE);
    assert!(staged.contains(&std::process::id().to_string()), "{staged}");
    assert!(staged.ends_with(".tmp"), "still recognisable as staging");
    assert!(
        staged.rsplit('/').next().unwrap().starts_with('.'),
        "still hidden, so a killed upload is not mistaken for an install"
    );
    assert_eq!(
        staged.rsplit_once('/').unwrap().0,
        BINARY.rsplit_once('/').unwrap().0,
        "same directory, so the publishing rename is still atomic"
    );
}

#[test]
fn replacing_reuses_a_published_binary_that_already_serves_us() {
    let (remote, legacy) = FakeRemote::new().with_legacy_install("26.7.4");
    let remote = remote.with_previous_install().serving(&legacy).speaking(
        &legacy,
        RemoteProtocol {
            control: CONTROL - 1,
            protocol: PROTOCOL,
            build: "26.7.4".to_string(),
        },
    );
    let release = FakeRelease::new();
    let user = FakeUser::declining();

    installer(&remote, &release, &user, "me@stuck-box:22")
        .replace()
        .expect("the binary is already there and it speaks to us");

    assert!(
        release.fetched().is_empty(),
        "nothing to download: {:?}",
        release.fetched()
    );
    assert!(
        !remote
            .writes()
            .iter()
            .any(|j| matches!(j, Journal::Put { .. })),
        "and nothing to upload: {:?}",
        remote.writes()
    );
    assert!(
        remote
            .journal()
            .iter()
            .any(|j| matches!(j, Journal::Launch)),
        "but the daemon really is restarted: {:?}",
        remote.journal()
    );
}

#[test]
fn replacing_overwrites_a_published_binary_that_does_not_serve_us() {
    let remote = FakeRemote::new().with_previous_install();
    let remote = remote.speaking(
        BINARY,
        RemoteProtocol {
            control: CONTROL - 1,
            protocol: PROTOCOL,
            build: "hand-placed".to_string(),
        },
    );
    let release = FakeRelease::new();
    let user = FakeUser::declining();

    installer(&remote, &release, &user, "me@tampered-box:22")
        .replace()
        .expect("ours is written over it");

    assert!(
        remote
            .writes()
            .iter()
            .any(|j| matches!(j, Journal::Put { .. })),
        "the file had to be rewritten: {:?}",
        remote.writes()
    );
    assert!(!release.fetched().is_empty(), "which means downloading it");
}

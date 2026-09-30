//! The installer against a fake Windows OpenSSH host.
//!
//! The fake answers the way such a host does where it matters: `uname` is not
//! a command, SFTP spells paths `/C:/…` and reports no execute bits, a rename
//! onto an existing file fails, and the running server's image cannot be
//! deleted. Every other command has to arrive as one of `windows_host`'s
//! encoded PowerShell scripts — anything else is recorded as foreign, and the
//! tests fail on it, because a POSIX command reaching `cmd.exe` is exactly the
//! bug this suite exists to catch.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::asset::{
    ASSET_WINDOWS_AARCH64, ASSET_WINDOWS_X86_64, CHECKSUMS_ASSET, RemotePlatform, UnsupportedTarget,
};
use super::*;

const VERSION: &str = "26.9.9";
const CONTROL: u32 = 3;
const PROTOCOL: u32 = 4;
const HOME: &str = "/C:/Users/me";
const BIN_DIR: &str = "/C:/Users/me/AppData/Local/tty7/bin";
const BINARY: &str = "/C:/Users/me/AppData/Local/tty7/bin/tty7-server-c3p4.exe";
const SERVER_BYTES: &[u8] = b"MZ\x90\x00...a tty7-server.exe, pretend it is 8 MB";

fn ours() -> RemoteProtocol {
    RemoteProtocol {
        control: CONTROL,
        protocol: PROTOCOL,
        build: VERSION.to_string(),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Step {
    /// `uname -sm`, which the fake refuses the way cmd.exe does.
    Uname,
    /// A `windows_host` script, by its tag.
    Script(String),
    Mkdir(String),
    Chmod(String),
    Put(String),
    Rename {
        from: String,
        to: String,
    },
    Remove(String),
    /// Anything a Windows shell would not have understood.
    Foreign(String),
}

struct FakeWindows {
    home: String,
    /// `None` is cmd.exe or PowerShell: no `uname` at all.
    uname: Option<String>,
    /// `None`: PowerShell does not answer either (not Windows after all).
    arch: Option<String>,
    files: Mutex<HashMap<String, (Vec<u8>, bool)>>,
    speaks: Mutex<HashMap<String, RemoteProtocol>>,
    uploads_speak: RemoteProtocol,
    /// The image of the running daemon, in SFTP spelling.
    running: Mutex<Option<String>>,
    steps: Mutex<Vec<Step>>,
}

impl FakeWindows {
    fn new(arch: &str) -> Self {
        let mut files = HashMap::new();
        files.insert(HOME.to_string(), (Vec::new(), true));
        Self {
            home: HOME.to_string(),
            uname: None,
            arch: Some(arch.to_string()),
            files: Mutex::new(files),
            speaks: Mutex::new(HashMap::new()),
            uploads_speak: ours(),
            running: Mutex::new(None),
            steps: Mutex::new(Vec::new()),
        }
    }

    fn with_uname(mut self, uname: &str) -> Self {
        self.uname = Some(uname.to_string());
        self
    }

    fn not_answering_powershell(mut self) -> Self {
        self.arch = None;
        self
    }

    fn installed(self, path: &str, spoken: RemoteProtocol) -> Self {
        {
            let mut files = self.files.lock().unwrap();
            for dir in
                asset::remote_paths_on(RemotePlatform::Windows, HOME, CONTROL, PROTOCOL).dir_chain
            {
                files.insert(dir, (Vec::new(), true));
            }
            files.insert(path.to_string(), (SERVER_BYTES.to_vec(), false));
        }
        self.speaks.lock().unwrap().insert(path.to_string(), spoken);
        self
    }

    fn serving(self, image: &str) -> Self {
        *self.running.lock().unwrap() = Some(image.to_string());
        self
    }

    fn steps(&self) -> Vec<Step> {
        self.steps.lock().unwrap().clone()
    }

    fn scripts(&self) -> Vec<String> {
        self.steps()
            .into_iter()
            .filter_map(|s| match s {
                Step::Script(tag) => Some(tag),
                _ => None,
            })
            .collect()
    }

    fn assert_nothing_foreign(&self) {
        let foreign: Vec<Step> = self
            .steps()
            .into_iter()
            .filter(|s| matches!(s, Step::Foreign(_)))
            .collect();
        assert!(
            foreign.is_empty(),
            "a Windows host was sent commands no Windows shell runs: {foreign:?}"
        );
    }

    fn has(&self, path: &str) -> bool {
        self.files.lock().unwrap().contains_key(path)
    }

    fn step(&self, step: Step) {
        self.steps.lock().unwrap().push(step);
    }
}

fn ok(stdout: &str) -> Result<ExecOutput, String> {
    Ok(ExecOutput {
        status: Some(0),
        stdout: stdout.to_string(),
        stderr: String::new(),
    })
}

fn failed(status: u32, stderr: &str) -> Result<ExecOutput, String> {
    Ok(ExecOutput {
        status: Some(status),
        stdout: String::new(),
        stderr: stderr.to_string(),
    })
}

/// The native path a script names as `& '<path>' <flag>`, back in SFTP form.
fn invoked(script: &str, flag: &str) -> Option<String> {
    let (before, _) = script.split_once(&format!("' {flag}"))?;
    let native = before.rsplit_once("& '")?.1;
    asset::sftp_path_from_windows(native)
}

impl RemoteOps for FakeWindows {
    fn home_dir(&self) -> Result<String, String> {
        Ok(self.home.clone())
    }

    fn run(&self, cmd: &str) -> Result<ExecOutput, String> {
        if cmd == "uname -sm" {
            self.step(Step::Uname);
            return match &self.uname {
                Some(uname) => ok(uname),
                None => failed(
                    1,
                    "'uname' is not recognized as an internal or external command,\r\n\
                     operable program or batch file.\r\n",
                ),
            };
        }
        let Some((tag, script)) = windows_host::decode(cmd) else {
            self.step(Step::Foreign(cmd.to_string()));
            return failed(1, "is not recognized as an internal or external command");
        };
        self.step(Step::Script(tag.clone()));
        match tag.as_str() {
            "probe" => match &self.arch {
                Some(arch) => ok(&format!("{} {arch}\r\n", windows_host::PROBE_MARK)),
                None => Err("powershell.exe: command not found".into()),
            },
            "protocol" => {
                let exe = invoked(&script, PROTOCOL_FLAG).expect("a native path to probe");
                match self.speaks.lock().unwrap().get(&exe) {
                    Some(spoken) => ok(&format!("{}\r\n", spoken.to_line())),
                    None => failed(1, "The term is not recognized"),
                }
            }
            "control-probe" => match self.running.lock().unwrap().is_some() {
                true => ok(""),
                false => failed(
                    1,
                    "tty7-server: stdio session ended with error: no daemon port file",
                ),
            },
            "running-exe" => ok(&self
                .running
                .lock()
                .unwrap()
                .as_deref()
                .and_then(asset::windows_native_path)
                .unwrap_or_default()),
            "stop" => {
                assert!(
                    invoked(&script, "--stop").is_some(),
                    "the stop goes through a server binary: {script}"
                );
                *self.running.lock().unwrap() = None;
                ok("")
            }
            "launch" => {
                assert!(
                    script.contains("Win32_Process -MethodName Create"),
                    "{script}"
                );
                *self.running.lock().unwrap() = Some(BINARY.to_string());
                ok("")
            }
            "read-exit" | "tail" => ok(""),
            other => panic!("the fake does not know the `{other}` script"),
        }
    }

    fn spawn_detached(&self, cmd: &str) -> Result<(), String> {
        self.run(cmd).map(|_| ())
    }

    fn stat(&self, path: &str) -> Result<Option<RemoteStat>, String> {
        // Windows OpenSSH reports no execute bits for an .exe.
        Ok(self
            .files
            .lock()
            .unwrap()
            .get(path)
            .map(|(bytes, is_dir)| RemoteStat {
                size: bytes.len() as u64,
                mode: if *is_dir { 0o40755 } else { 0o100644 },
                is_dir: *is_dir,
            }))
    }

    fn mkdir(&self, path: &str) -> Result<(), String> {
        self.step(Step::Mkdir(path.to_string()));
        assert!(asset::is_windows_sftp_path(path), "{path}");
        self.files
            .lock()
            .unwrap()
            .entry(path.to_string())
            .or_insert((Vec::new(), true));
        Ok(())
    }

    fn chmod(&self, path: &str, _mode: u32) -> Result<(), String> {
        self.step(Step::Chmod(path.to_string()));
        Ok(())
    }

    fn put(&self, path: &str, bytes: &[u8]) -> Result<(), String> {
        self.step(Step::Put(path.to_string()));
        assert!(
            path.ends_with(".exe"),
            "an upload is run before it is renamed: {path}"
        );
        self.files
            .lock()
            .unwrap()
            .insert(path.to_string(), (bytes.to_vec(), false));
        self.speaks
            .lock()
            .unwrap()
            .insert(path.to_string(), self.uploads_speak.clone());
        Ok(())
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), String> {
        self.step(Step::Rename {
            from: from.to_string(),
            to: to.to_string(),
        });
        let mut files = self.files.lock().unwrap();
        // Windows OpenSSH renames without replacing.
        if files.contains_key(to) {
            return Err("4: Failure".into());
        }
        let file = files.remove(from).ok_or("2: No such file")?;
        files.insert(to.to_string(), file);
        let mut speaks = self.speaks.lock().unwrap();
        if let Some(spoken) = speaks.remove(from) {
            speaks.insert(to.to_string(), spoken);
        }
        // A running image keeps running from wherever it is moved to.
        let mut running = self.running.lock().unwrap();
        if running.as_deref() == Some(from) {
            *running = Some(to.to_string());
        }
        Ok(())
    }

    fn remove_file(&self, path: &str) -> Result<(), String> {
        self.step(Step::Remove(path.to_string()));
        if self.running.lock().unwrap().as_deref() == Some(path) {
            return Err("4: Failure (the image is in use)".into());
        }
        self.files.lock().unwrap().remove(path);
        Ok(())
    }

    fn list_dir(&self, path: &str) -> Result<Option<Vec<String>>, String> {
        let files = self.files.lock().unwrap();
        if !files.get(path).is_some_and(|(_, dir)| *dir) {
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

struct Release {
    fetched: Mutex<Vec<String>>,
}

impl Release {
    fn new() -> Self {
        Self {
            fetched: Mutex::new(Vec::new()),
        }
    }
}

impl AssetFetcher for Release {
    fn get(&self, url: &str) -> Result<Vec<u8>, String> {
        self.fetched.lock().unwrap().push(url.to_string());
        let digest = checksums::hex(&checksums::sha256(SERVER_BYTES));
        if url.ends_with(CHECKSUMS_ASSET) {
            return Ok(format!(
                "{digest}  {ASSET_WINDOWS_X86_64}\n{digest}  {ASSET_WINDOWS_AARCH64}\n"
            )
            .into_bytes());
        }
        if url.ends_with(ASSET_WINDOWS_X86_64) || url.ends_with(ASSET_WINDOWS_AARCH64) {
            return Ok(SERVER_BYTES.to_vec());
        }
        Err(format!("404: {url}"))
    }
}

struct Approve(Mutex<Vec<InstallRequest>>);

impl InstallConfirm for Approve {
    fn confirm(&self, request: &InstallRequest) -> InstallDecision {
        self.0.lock().unwrap().push(request.clone());
        InstallDecision::Approve
    }
}

fn installer<'a>(host: &'a FakeWindows, release: &'a Release, user: &'a Approve) -> Installer<'a> {
    Installer::new(host, release, user, "win-box")
        .with_version(VERSION)
        .with_dialect(CONTROL, PROTOCOL)
        .with_timeouts(Duration::from_millis(200), Duration::from_millis(10))
        .with_shutdown_timeout(Duration::from_millis(200))
}

#[test]
fn a_fresh_windows_host_gets_the_windows_server_in_local_app_data() {
    let host = FakeWindows::new("AMD64");
    let release = Release::new();
    let user = Approve(Mutex::new(Vec::new()));

    let report = installer(&host, &release, &user)
        .run()
        .expect("a Windows host installs");

    host.assert_nothing_foreign();
    assert_eq!(report.asset, ASSET_WINDOWS_X86_64);
    assert_eq!(report.paths.platform, RemotePlatform::Windows);
    assert_eq!(report.paths.binary, BINARY);
    assert!(report.installed && report.confirmed && report.launched);
    assert!(host.has(BINARY));

    let asked = user.0.lock().unwrap();
    assert_eq!(asked.len(), 1, "a first install is confirmed");
    assert_eq!(asked[0].remote_path, BINARY);
    assert_eq!(asked[0].asset, ASSET_WINDOWS_X86_64);
    assert!(
        release.fetched.lock().unwrap()[1].ends_with("/v26.9.9/tty7-server-windows-x86_64.exe"),
        "{:?}",
        release.fetched.lock().unwrap()
    );

    let steps = host.steps();
    assert_eq!(steps[0], Step::Uname, "unix is still asked first");
    assert_eq!(steps[1], Step::Script("probe".into()));
    assert!(
        !steps.iter().any(|s| matches!(s, Step::Chmod(_))),
        "NTFS has no mode bits to set: {steps:?}"
    );
    for dir in [
        "/C:/Users/me/AppData",
        "/C:/Users/me/AppData/Local",
        "/C:/Users/me/AppData/Local/tty7",
        BIN_DIR,
    ] {
        assert!(steps.contains(&Step::Mkdir(dir.into())), "{dir}: {steps:?}");
    }
    let temp = steps
        .iter()
        .find_map(|s| match s {
            Step::Put(path) => Some(path.clone()),
            _ => None,
        })
        .expect("an upload");
    assert!(
        temp.starts_with(&format!("{BIN_DIR}/.tty7-server-c3p4.")) && temp.ends_with(".tmp.exe"),
        "{temp}"
    );
    assert!(steps.contains(&Step::Rename {
        from: temp.clone(),
        to: BINARY.into()
    }));

    let scripts = host.scripts();
    let launch = scripts
        .iter()
        .position(|t| t == "launch")
        .expect("a launch");
    let protocol = scripts
        .iter()
        .position(|t| t == "protocol")
        .expect("a probe");
    assert!(
        protocol < launch,
        "the upload is proven before it is started: {scripts:?}"
    );
    assert_eq!(scripts.last().map(String::as_str), Some("running-exe"));
}

#[test]
fn an_arm64_host_gets_the_arm64_server() {
    let host = FakeWindows::new("ARM64");
    let release = Release::new();
    let user = Approve(Mutex::new(Vec::new()));
    let report = installer(&host, &release, &user).run().unwrap();
    assert_eq!(report.asset, ASSET_WINDOWS_AARCH64);
    host.assert_nothing_foreign();
}

#[test]
fn a_32_bit_windows_is_refused_before_anything_is_written() {
    let host = FakeWindows::new("x86");
    let release = Release::new();
    let user = Approve(Mutex::new(Vec::new()));
    let err = installer(&host, &release, &user).run().unwrap_err();
    assert!(
        matches!(
            err,
            InstallError::Unsupported(UnsupportedTarget::UnknownWindowsMachine { .. })
        ),
        "{err:?}"
    );
    assert!(err.to_string().contains("\"x86\""), "{err}");
    assert!(release.fetched.lock().unwrap().is_empty());
    assert!(!host.has(BIN_DIR));
}

#[test]
fn git_for_windows_on_the_path_still_gets_the_windows_server() {
    let host = FakeWindows::new("AMD64").with_uname("MINGW64_NT-10.0-26100 x86_64\n");
    let release = Release::new();
    let user = Approve(Mutex::new(Vec::new()));
    let report = installer(&host, &release, &user).run().unwrap();
    assert_eq!(report.asset, ASSET_WINDOWS_X86_64);
    assert_eq!(report.paths.binary, BINARY);
    host.assert_nothing_foreign();
}

/// Neither probe answering is a machine that is neither, and `uname`'s
/// failure is the one worth reading.
#[test]
fn when_nothing_answers_the_uname_failure_is_reported() {
    let host = FakeWindows::new("AMD64").not_answering_powershell();
    let release = Release::new();
    let user = Approve(Mutex::new(Vec::new()));
    let err = installer(&host, &release, &user).run().unwrap_err();
    match err {
        InstallError::Probe(reason) => {
            assert!(reason.contains("'uname' is not recognized"), "{reason}")
        }
        other => panic!("expected the uname failure, got {other:?}"),
    }
}

/// A Windows host whose OpenSSH `DefaultShell` is WSL's bash answers `uname`
/// as Linux while SFTP writes to `C:\`. A Linux server installed at `/C:/…`
/// would be a file no Linux path reaches.
#[test]
fn a_linux_shell_over_windows_sftp_is_refused() {
    let host = FakeWindows::new("AMD64").with_uname("Linux x86_64\n");
    let release = Release::new();
    let user = Approve(Mutex::new(Vec::new()));
    let err = installer(&host, &release, &user).run().unwrap_err();
    let InstallError::Probe(reason) = &err else {
        panic!("expected a probe error, got {err:?}");
    };
    assert!(reason.contains("DefaultShell"), "{reason}");
    assert!(release.fetched.lock().unwrap().is_empty());
    assert!(!host.has(BIN_DIR), "nothing was written");
}

#[test]
fn a_serving_windows_host_is_left_alone() {
    let host = FakeWindows::new("AMD64")
        .installed(BINARY, ours())
        .serving(BINARY);
    let release = Release::new();
    let user = Approve(Mutex::new(Vec::new()));
    let report = installer(&host, &release, &user).run().unwrap();
    assert!(!report.installed && !report.launched);
    assert_eq!(report.mismatch, None);
    assert!(
        release.fetched.lock().unwrap().is_empty(),
        "an .exe with no execute bit over SFTP is still an installed server"
    );
    host.assert_nothing_foreign();
}

/// The running image cannot be deleted on Windows, but it can be renamed, and
/// that is how an update gets its name back.
#[test]
fn an_update_moves_the_running_image_aside_and_the_next_one_sweeps_it() {
    let host = FakeWindows::new("AMD64")
        .installed(BINARY, ours())
        .serving(BINARY);
    let release = Release::new();
    let user = Approve(Mutex::new(Vec::new()));

    installer(&host, &release, &user)
        .replace_forced()
        .expect("the update goes through");
    host.assert_nothing_foreign();

    let steps = host.steps();
    let aside = steps
        .iter()
        .find_map(|s| match s {
            Step::Rename { from, to } if from == BINARY => Some(to.clone()),
            _ => None,
        })
        .expect("the running image was moved aside");
    assert!(
        aside.starts_with(&format!("{BIN_DIR}/.tty7-server-c3p4.exe.")) && aside.ends_with(".old"),
        "{aside}"
    );
    assert!(host.has(BINARY), "the new server took the name");
    assert!(
        host.has(&aside),
        "the old image is still there until it stops running"
    );

    let scripts = host.scripts();
    let stop = scripts.iter().position(|t| t == "stop").expect("a stop");
    let launch = scripts
        .iter()
        .rposition(|t| t == "launch")
        .expect("a start");
    assert!(stop < launch, "{scripts:?}");
    assert!(
        user.0.lock().unwrap().is_empty(),
        "an update is not a first install"
    );

    // The old image has stopped; the next update clears it away.
    installer(&host, &release, &user).replace_forced().unwrap();
    assert!(!host.has(&aside), "the moved-aside image was swept");
}

#[test]
fn a_restart_on_windows_asks_the_server_to_stop_instead_of_signalling_it() {
    let host = FakeWindows::new("AMD64")
        .installed(BINARY, ours())
        .serving(BINARY);
    let release = Release::new();
    let user = Approve(Mutex::new(Vec::new()));
    installer(&host, &release, &user).restart_daemon().unwrap();

    host.assert_nothing_foreign();
    let steps = host.steps();
    assert!(
        !steps.contains(&Step::Uname),
        "a restart knows the platform from the SFTP home: {steps:?}"
    );
    let scripts = host.scripts();
    assert!(
        scripts.windows(2).any(|w| w == ["stop", "control-probe"]),
        "{scripts:?}"
    );
    assert!(scripts.contains(&"launch".to_string()), "{scripts:?}");
}

#[test]
fn another_builds_daemon_is_reported_in_the_installers_spelling() {
    let other = "/C:/Users/me/AppData/Local/tty7/bin/tty7-server-c2p4.exe";
    let host = FakeWindows::new("AMD64")
        .installed(BINARY, ours())
        .serving(other);
    host.speaks.lock().unwrap().insert(
        other.to_string(),
        RemoteProtocol {
            control: 2,
            protocol: 4,
            build: "26.8.1".into(),
        },
    );
    let release = Release::new();
    let user = Approve(Mutex::new(Vec::new()));
    let sink = Arc::new(Mutex::new(Vec::new()));
    let report =
        with_mismatch_sink(sink.clone(), || installer(&host, &release, &user).run()).unwrap();

    let mismatch = report.mismatch.expect("the other build is reported");
    assert_eq!(mismatch.running_exe.as_deref(), Some(other));
    assert_eq!(mismatch.running_version.as_deref(), Some("26.8.1"));
    assert_eq!(sink.lock().unwrap().len(), 1);
    host.assert_nothing_foreign();
}

#[test]
fn the_link_command_is_phrased_for_the_hosts_shell() {
    assert_eq!(
        server_stdio_command(BINARY),
        r#""C:\Users\me\AppData\Local\tty7\bin\tty7-server-c3p4.exe" --stdio"#
    );
    assert_eq!(
        server_stdio_command("/home/me/.local/share/tty7/bin/tty7-server-c3p4"),
        "'/home/me/.local/share/tty7/bin/tty7-server-c3p4' --stdio",
        "the unix command is unchanged"
    );
}

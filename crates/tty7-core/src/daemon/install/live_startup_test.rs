//! Runs on the authorized Linux test server, never the user's default config.
use super::*;
use std::path::{Path, PathBuf};

struct LocalShell {
    root: PathBuf,
    config: PathBuf,
}
impl RemoteOps for LocalShell {
    fn home_dir(&self) -> Result<String, String> {
        Ok(self.root.to_string_lossy().into_owned())
    }
    fn run(&self, command: &str) -> Result<ExecOutput, String> {
        let output = std::process::Command::new("sh")
            .args(["-c", command])
            .env("TTY7_CONFIG_DIR", &self.config)
            .env("TTY7_DATA_DIR", self.root.join("data"))
            .output()
            .map_err(|e| e.to_string())?;
        Ok(ExecOutput {
            status: output.status.code().map(|code| code as u32),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
    fn spawn_detached(&self, command: &str) -> Result<(), String> {
        let result = self.run(command)?;
        if result.success() {
            Ok(())
        } else {
            Err(result.failure_reason())
        }
    }
    fn stat(&self, _: &str) -> Result<Option<RemoteStat>, String> {
        unreachable!()
    }
    fn mkdir(&self, _: &str) -> Result<(), String> {
        unreachable!()
    }
    fn chmod(&self, _: &str, _: u32) -> Result<(), String> {
        unreachable!()
    }
    fn put(&self, _: &str, _: &[u8]) -> Result<(), String> {
        unreachable!()
    }
    fn rename(&self, _: &str, _: &str) -> Result<(), String> {
        unreachable!()
    }
    fn remove_file(&self, _: &str) -> Result<(), String> {
        unreachable!()
    }
    fn list_dir(&self, _: &str) -> Result<Option<Vec<String>>, String> {
        unreachable!()
    }
}
impl Drop for LocalShell {
    fn drop(&mut self) {
        // Only the fixture's private endpoint; no process-name scans or signals.
        if let Ok(mut socket) =
            std::os::unix::net::UnixStream::connect(self.config.join("daemon.sock"))
        {
            use crate::daemon::protocol::{ClientMsg, PaneAccess};
            let _ = ClientMsg::Access(PaneAccess::Manage).encode(&mut socket);
            let _ = ClientMsg::Shutdown.encode(&mut socket);
            let deadline = Instant::now() + Duration::from_secs(5);
            while self.config.join("daemon.sock").exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

#[test]
#[ignore = "requires TTY7_TEST_SERVER pointing to the isolated Linux candidate"]
fn real_startup_failure_is_fast_diagnostic_and_retryable_without_a_half_daemon() {
    let candidate =
        std::env::var("TTY7_TEST_SERVER").expect("explicit isolated server path required");
    let candidate = Path::new(&candidate).canonicalize().unwrap();
    assert!(candidate.starts_with("/tmp") && candidate.to_string_lossy().contains("tty7-774-"));
    let root = tempfile::Builder::new()
        .prefix("startup-")
        .tempdir_in(candidate.parent().unwrap())
        .unwrap();
    let shell = LocalShell {
        root: root.path().to_path_buf(),
        config: root.path().join("config"),
    };
    std::fs::create_dir_all(shell.config.join("control.sock")).unwrap();
    let fetch = FakeRelease::new();
    let user = FakeUser::declining();
    let installer = Installer::new(&shell, &fetch, &user, "private-linux-startup")
        .with_timeouts(Duration::from_secs(5), Duration::from_millis(30));
    let paths = installer.paths_for(&root.path().to_string_lossy());
    std::fs::create_dir_all(&paths.bin_dir).unwrap();
    std::fs::copy(&candidate, &paths.binary).unwrap();
    let begin = Instant::now();
    let error = installer.ensure_daemon(&paths).unwrap_err().to_string();
    eprintln!("failed start in {:?}: {error}", begin.elapsed());
    assert!(begin.elapsed() < Duration::from_secs(3));
    assert!(
        error.contains("control listener unavailable") && error.contains("TTY7_STARTUP_EXIT=1"),
        "{error}"
    );
    assert!(
        !shell.config.join("daemon.sock").exists(),
        "no half-started daemon retains the seat"
    );
    // Preserve the fixture's conflicting directory for inspection.
    std::fs::rename(
        shell.config.join("control.sock"),
        shell.config.join("control.sock.conflict"),
    )
    .unwrap();
    assert!(installer.ensure_daemon(&paths).unwrap().0);
    assert!(
        !installer.ensure_daemon(&paths).unwrap().0,
        "a second connect reuses the verified instance"
    );
    installer
        .check_health(&paths, false, Duration::from_secs(3))
        .unwrap();
    eprintln!(
        "retry after removing the fixture conflict: healthy same-instance endpoints, no repeated launch"
    );
    drop(installer);
    drop(shell);
    // Keep the test's bounded startup logs for follow-up inspection.
    let _ = root.keep();
}

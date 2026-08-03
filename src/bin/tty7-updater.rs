#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

#[cfg(target_os = "macos")]
mod macos {
    use std::fs::{self, OpenOptions};
    use std::io::Write as _;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::thread;
    use std::time::Duration;

    const PARENT_POLL: Duration = Duration::from_millis(100);
    const LAUNCH_GRACE: Duration = Duration::from_secs(1);

    pub fn run() -> Result<(), String> {
        let mut args = std::env::args_os().skip(1);
        let command = args
            .next()
            .and_then(|arg| arg.into_string().ok())
            .ok_or_else(usage)?;
        match command.as_str() {
            "verify" => {
                let current = next_path(&mut args)?;
                let archive = next_path(&mut args)?;
                let checksums = next_path(&mut args)?;
                let asset_name = next_string(&mut args)?;
                let stage = next_path(&mut args)?;
                let expected_version = next_string(&mut args)?;
                reject_extra(args)?;
                verify_archive(&archive, &checksums, &asset_name)?;
                let replacement = extract_archive(&archive, &stage)?;
                verify_update(&current, &replacement, &expected_version)
            }
            "install" => {
                let parent_pid = next_string(&mut args)?
                    .parse::<u32>()
                    .map_err(|_| "parent pid is not an unsigned integer".to_string())?;
                let current = next_path(&mut args)?;
                let stage = next_path(&mut args)?;
                let expected_version = next_string(&mut args)?;
                let log = next_path(&mut args)?;
                reject_extra(args)?;
                install(InstallPlan {
                    parent_pid,
                    current,
                    stage,
                    expected_version,
                    log,
                })
            }
            _ => Err(usage()),
        }
    }

    fn usage() -> String {
        "usage: tty7-updater verify <current.app> <archive.zip> <checksums.txt> \
         <asset-name> <stage-dir> <version>\n\
         or: tty7-updater install <parent-pid> <current.app> <stage-dir> <version> <log-path>"
            .to_string()
    }

    fn next_path(args: &mut impl Iterator<Item = std::ffi::OsString>) -> Result<PathBuf, String> {
        args.next().map(PathBuf::from).ok_or_else(usage)
    }

    fn next_string(args: &mut impl Iterator<Item = std::ffi::OsString>) -> Result<String, String> {
        args.next()
            .and_then(|arg| arg.into_string().ok())
            .ok_or_else(usage)
    }

    fn reject_extra(mut args: impl Iterator<Item = std::ffi::OsString>) -> Result<(), String> {
        if args.next().is_some() {
            Err(usage())
        } else {
            Ok(())
        }
    }

    struct InstallPlan {
        parent_pid: u32,
        current: PathBuf,
        stage: PathBuf,
        expected_version: String,
        log: PathBuf,
    }

    fn install(plan: InstallPlan) -> Result<(), String> {
        log_line(&plan.log, "re-verifying staged tty7 update");
        let replacement = plan.stage.join("unpacked/tty7.app");
        wait_for_exit(plan.parent_pid);
        if let Err(error) = verify_update(&plan.current, &replacement, &plan.expected_version) {
            log_line(&plan.log, &error);
            let _ = fs::remove_dir_all(&plan.stage);
            let _ = launch_app(&plan.current);
            return Err(error);
        }
        log_line(&plan.log, &format!("replacing {}", plan.current.display()));
        replace_and_relaunch(&plan.current, &replacement, &plan.stage, launch_app)
            .inspect_err(|error| log_line(&plan.log, error))
    }

    fn verify_archive(archive: &Path, checksums: &Path, asset_name: &str) -> Result<(), String> {
        let bytes =
            fs::read(archive).map_err(|error| format!("reading {}: {error}", archive.display()))?;
        let manifest = fs::read_to_string(checksums)
            .map_err(|error| format!("reading {}: {error}", checksums.display()))?;
        tty7_core::daemon::install::checksums::verify(&manifest, asset_name, &bytes)
            .map_err(|error| error.to_string())
    }

    fn extract_archive(archive: &Path, stage: &Path) -> Result<PathBuf, String> {
        let unpacked = stage.join("unpacked");
        fs::create_dir(&unpacked)
            .map_err(|error| format!("creating {}: {error}", unpacked.display()))?;
        run_checked(
            Command::new("/usr/bin/ditto")
                .args(["-x", "-k"])
                .arg(archive)
                .arg(&unpacked),
            "extracting the update archive",
        )?;
        Ok(unpacked.join("tty7.app"))
    }

    fn verify_update(
        current: &Path,
        replacement: &Path,
        expected_version: &str,
    ) -> Result<(), String> {
        let executable = replacement.join("Contents/MacOS/tty7-app");
        let updater = replacement.join("Contents/MacOS/tty7-updater");
        if !replacement.is_dir() || !executable.is_file() || !updater.is_file() {
            return Err(
                "the staged bundle is missing tty7-app or tty7-updater under Contents/MacOS"
                    .to_string(),
            );
        }
        let actual_version = bundle_version(replacement)?;
        if actual_version != expected_version {
            return Err(format!(
                "the staged app reports version {actual_version}, expected {expected_version}"
            ));
        }
        run_checked(
            Command::new("/usr/bin/codesign")
                .args(["--verify", "--deep", "--strict"])
                .arg(replacement),
            "verifying the staged app's code signature",
        )?;
        let current_requirement = signing_requirement(current)?;
        let replacement_requirement = signing_requirement(replacement)?;
        if current_requirement != replacement_requirement {
            return Err(format!(
                "the staged app has a different designated requirement: current \
                 {current_requirement:?}, staged {replacement_requirement:?}"
            ));
        }
        Ok(())
    }

    fn bundle_version(app: &Path) -> Result<String, String> {
        let output = Command::new("/usr/libexec/PlistBuddy")
            .args(["-c", "Print :CFBundleShortVersionString"])
            .arg(app.join("Contents/Info.plist"))
            .output()
            .map_err(|error| format!("reading the staged app version: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "reading the staged app version: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    fn signing_requirement(app: &Path) -> Result<String, String> {
        let output = Command::new("/usr/bin/codesign")
            .args(["-d", "-r-"])
            .arg(app)
            .output()
            .map_err(|error| {
                format!(
                    "reading the code-signing requirement for {}: {error}",
                    app.display()
                )
            })?;
        if !output.status.success() {
            return Err(format!(
                "reading the code-signing requirement for {}: {}",
                app.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        String::from_utf8_lossy(&output.stderr)
            .lines()
            .find_map(|line| line.strip_prefix("designated => ").map(str::to_string))
            .ok_or_else(|| "codesign did not report a designated requirement".to_string())
    }

    fn replace_and_relaunch(
        current: &Path,
        replacement: &Path,
        stage: &Path,
        launch: impl Fn(&Path) -> Result<(), String>,
    ) -> Result<(), String> {
        // The staging directory is a fresh TempDir created beside the current
        // bundle, so a backup here stays on the same filesystem without using a
        // predictable sibling path.  In particular, never delete a fixed-name
        // path beside the app: it may be a recovery copy left by an interrupted
        // update (or simply an unrelated user-owned path).
        let backup = stage.join("previous.app");
        if backup.exists() {
            return Err(format!(
                "the update staging backup already exists: {}",
                backup.display()
            ));
        }
        fs::rename(current, &backup)
            .map_err(|error| format!("moving the current app aside: {error}"))?;

        if let Err(error) = fs::rename(replacement, current) {
            let _ = fs::rename(&backup, current);
            let _ = fs::remove_dir_all(stage);
            return Err(format!("putting the staged app in place: {error}"));
        }

        match launch(current) {
            Ok(()) => {
                let _ = remove_path(&backup);
                let _ = fs::remove_dir_all(stage);
                Ok(())
            }
            Err(error) => {
                let _ = remove_path(current);
                fs::rename(&backup, current)
                    .map_err(|restore| format!("{error}; restoring the previous app: {restore}"))?;
                let _ = fs::remove_dir_all(stage);
                let _ = launch(current);
                Err(error)
            }
        }
    }

    fn launch_app(app: &Path) -> Result<(), String> {
        let executable = app.join("Contents/MacOS/tty7-app");
        let mut child = Command::new(&executable)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("launching {}: {error}", executable.display()))?;
        healthy_after_grace(&mut child)
    }

    fn healthy_after_grace(child: &mut Child) -> Result<(), String> {
        thread::sleep(LAUNCH_GRACE);
        match child
            .try_wait()
            .map_err(|error| format!("checking the relaunched app: {error}"))?
        {
            None => Ok(()),
            Some(status) => Err(format!(
                "the relaunched app exited immediately with {status}"
            )),
        }
    }

    fn wait_for_exit(pid: u32) {
        while process_alive(pid) {
            thread::sleep(PARENT_POLL);
        }
    }

    fn process_alive(pid: u32) -> bool {
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }

    fn remove_path(path: &Path) -> Result<(), String> {
        if !path.exists() {
            return Ok(());
        }
        if path.is_dir() {
            fs::remove_dir_all(path)
        } else {
            fs::remove_file(path)
        }
        .map_err(|error| format!("removing {}: {error}", path.display()))
    }

    fn run_checked(command: &mut Command, context: &str) -> Result<(), String> {
        let output = command
            .output()
            .map_err(|error| format!("{context}: {error}"))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "{context}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    }

    fn log_line(path: &Path, message: &str) {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(file, "{message}");
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn bundle(path: &Path, marker: &str) {
            fs::create_dir_all(path.join("Contents/MacOS")).unwrap();
            fs::write(path.join("marker"), marker).unwrap();
        }

        #[test]
        fn successful_launch_commits_the_replacement() {
            let root = tempfile::tempdir().unwrap();
            let current = root.path().join("tty7.app");
            let stage = root.path().join("stage");
            let replacement = stage.join("tty7.app");
            bundle(&current, "old");
            bundle(&replacement, "new");

            replace_and_relaunch(&current, &replacement, &stage, |_| Ok(())).unwrap();

            assert_eq!(fs::read_to_string(current.join("marker")).unwrap(), "new");
            assert!(!stage.exists());
            assert!(!root.path().join(".tty7.app.tty7-update-backup").exists());
        }

        #[test]
        fn failed_launch_restores_and_relaunches_the_previous_app() {
            let root = tempfile::tempdir().unwrap();
            let current = root.path().join("tty7.app");
            let stage = root.path().join("stage");
            let replacement = stage.join("tty7.app");
            bundle(&current, "old");
            bundle(&replacement, "new");
            let launches = std::cell::Cell::new(0);

            let error = replace_and_relaunch(&current, &replacement, &stage, |_| {
                launches.set(launches.get() + 1);
                if launches.get() == 1 {
                    Err("new app failed".to_string())
                } else {
                    Ok(())
                }
            })
            .unwrap_err();

            assert_eq!(error, "new app failed");
            assert_eq!(launches.get(), 2);
            assert_eq!(fs::read_to_string(current.join("marker")).unwrap(), "old");
            assert!(!stage.exists());
        }

        #[test]
        fn replacement_does_not_remove_a_fixed_name_sibling() {
            let root = tempfile::tempdir().unwrap();
            let current = root.path().join("tty7.app");
            let stage = root.path().join("stage");
            let replacement = stage.join("tty7.app");
            let sibling = root.path().join(".tty7.app.tty7-update-backup");
            bundle(&current, "old");
            bundle(&replacement, "new");
            bundle(&sibling, "keep");

            replace_and_relaunch(&current, &replacement, &stage, |_| Ok(())).unwrap();

            assert_eq!(fs::read_to_string(current.join("marker")).unwrap(), "new");
            assert_eq!(fs::read_to_string(sibling.join("marker")).unwrap(), "keep");
        }

        #[test]
        fn verify_rejects_a_bundle_without_the_helper() {
            let root = tempfile::tempdir().unwrap();
            let current = root.path().join("current.app");
            let replacement = root.path().join("replacement.app");
            bundle(&current, "old");
            fs::create_dir_all(replacement.join("Contents/MacOS")).unwrap();
            fs::write(replacement.join("Contents/MacOS/tty7-app"), b"app").unwrap();

            let error = verify_update(&current, &replacement, "1.0.0").unwrap_err();
            assert!(
                error.contains("missing tty7-app or tty7-updater"),
                "{error}"
            );
        }

        #[test]
        fn archive_verification_rejects_bytes_that_do_not_match_the_manifest() {
            let root = tempfile::tempdir().unwrap();
            let archive = root.path().join("tty7.zip");
            let manifest = root.path().join("checksums.txt");
            fs::write(&archive, b"downloaded bytes").unwrap();
            fs::write(
                &manifest,
                format!(
                    "{}  tty7.zip\n",
                    tty7_core::daemon::install::checksums::hex(
                        &tty7_core::daemon::install::checksums::sha256(b"published bytes")
                    )
                ),
            )
            .unwrap();

            let error = verify_archive(&archive, &manifest, "tty7.zip").unwrap_err();
            assert!(error.contains("failed sha256 verification"), "{error}");
        }
    }
}

#[cfg(target_os = "macos")]
fn main() {
    if let Err(error) = macos::run() {
        eprintln!("tty7-updater: {error}");
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("tty7-updater is only available on macOS");
    std::process::exit(1);
}

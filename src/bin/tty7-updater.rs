#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]
#![cfg_attr(
    not(any(target_os = "macos", target_os = "windows", target_os = "linux")),
    allow(dead_code)
)]

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
                let archive = next_path(&mut args)?;
                let checksums = next_path(&mut args)?;
                let asset_name = next_string(&mut args)?;
                let stage = next_path(&mut args)?;
                let expected_version = next_string(&mut args)?;
                let log = next_path(&mut args)?;
                let options = tail_options(args)?;
                options.apply();
                install(InstallPlan {
                    parent_pid,
                    current,
                    archive,
                    checksums,
                    asset_name,
                    stage,
                    expected_version,
                    log,
                    result_file: options.result_file,
                })
            }
            _ => Err(usage()),
        }
    }

    fn usage() -> String {
        "usage: tty7-updater verify <current.app> <archive.zip> <checksums.txt> \
         <asset-name> <stage-dir> <version>\n\
         or: tty7-updater install <parent-pid> <current.app> <archive.zip> <checksums.txt> \
         <asset-name> <stage-dir> <version> <log-path> \
         [--config-dir <dir>] [--result-file <path>]"
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

    /// The named options an install verb takes after its positional
    /// arguments. See the Windows half of this file for why these are
    /// arguments and not the environment.
    #[derive(Default)]
    struct TailOptions {
        config_dir: Option<PathBuf>,
        result_file: Option<PathBuf>,
    }

    fn tail_options(
        mut args: impl Iterator<Item = std::ffi::OsString>,
    ) -> Result<TailOptions, String> {
        let mut options = TailOptions::default();
        while let Some(arg) = args.next() {
            match arg.to_str() {
                Some("--config-dir") => options.config_dir = Some(next_path(&mut args)?),
                Some("--result-file") => options.result_file = Some(next_path(&mut args)?),
                _ => return Err(usage()),
            }
        }
        Ok(options)
    }

    impl TailOptions {
        fn apply(&self) {
            let Some(dir) = &self.config_dir else { return };
            tty7_core::core::config::set_config_dir(dir.clone());
            // Re-exported so the relaunched app — a child of this process —
            // keeps answering for the same config directory. Safe here:
            // argument parsing runs before any thread exists.
            unsafe { std::env::set_var("TTY7_CONFIG_DIR", dir) };
        }
    }

    /// The terminal outcome of the attempt, for the next GUI launch to merge
    /// into the update state (#540). Best-effort, like every log line here.
    fn report_outcome(
        result_file: Option<&Path>,
        log: &Path,
        version: &str,
        result: &Result<(), String>,
    ) {
        let Some(path) = result_file else { return };
        let outcome = tty7_core::daemon::install::outcome::UpdateOutcome {
            version: version.to_string(),
            ok: result.is_ok(),
            detail: result.as_ref().err().cloned(),
        };
        if let Err(error) = tty7_core::daemon::install::outcome::write_outcome(path, &outcome) {
            log_line(
                log,
                &format!(
                    "could not record the update outcome at {}: {error}",
                    path.display()
                ),
            );
        }
    }

    struct InstallPlan {
        parent_pid: u32,
        current: PathBuf,
        archive: PathBuf,
        checksums: PathBuf,
        asset_name: String,
        stage: PathBuf,
        expected_version: String,
        log: PathBuf,
        result_file: Option<PathBuf>,
    }

    fn install(plan: InstallPlan) -> Result<(), String> {
        install_inner(&plan)
    }

    fn install_inner(plan: &InstallPlan) -> Result<(), String> {
        let replacement = plan.stage.join("unpacked/tty7.app");
        wait_for_exit(plan.parent_pid);
        log_line(&plan.log, "re-verifying staged tty7 update");
        let verification = verify_archive(&plan.archive, &plan.checksums, &plan.asset_name)
            .and_then(|()| verify_update(&plan.current, &replacement, &plan.expected_version));
        if let Err(error) = verification {
            log_line(&plan.log, &error);
            let _ = fs::remove_dir_all(&plan.stage);
            let result = Err(error);
            // The outcome lands before the old app does: the relaunched GUI
            // merges it at startup, and a write afterward races that merge
            // (#540).
            report_outcome(
                plan.result_file.as_deref(),
                &plan.log,
                &plan.expected_version,
                &result,
            );
            let _ = launch_app(&plan.current);
            return result;
        }
        log_line(&plan.log, &format!("replacing {}", plan.current.display()));
        let report = |result: &Result<(), String>| {
            report_outcome(
                plan.result_file.as_deref(),
                &plan.log,
                &plan.expected_version,
                result,
            );
        };
        replace_and_relaunch(&plan.current, &replacement, &plan.stage, launch_app, report)
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
        report: impl Fn(&Result<(), String>),
    ) -> Result<(), String> {
        // The staging directory is a fresh TempDir created beside the current
        // bundle, so a backup here stays on the same filesystem without using a
        // predictable sibling path.  In particular, never delete a fixed-name
        // path beside the app: it may be a recovery copy left by an interrupted
        // update (or simply an unrelated user-owned path).
        let backup = stage.join("previous.app");
        if backup.exists() {
            let result = Err(format!(
                "the update staging backup already exists: {}",
                backup.display()
            ));
            report(&result);
            return result;
        }
        if let Err(error) = fs::rename(current, &backup) {
            let result = Err(format!("moving the current app aside: {error}"));
            report(&result);
            return result;
        }

        if let Err(error) = fs::rename(replacement, current) {
            let _ = fs::rename(&backup, current);
            let _ = fs::remove_dir_all(stage);
            let result = Err(format!("putting the staged app in place: {error}"));
            report(&result);
            return result;
        }

        match launch(current) {
            Ok(()) => {
                let _ = remove_path(&backup);
                let _ = fs::remove_dir_all(stage);
                let result = Ok(());
                report(&result);
                result
            }
            Err(error) => {
                let _ = remove_path(current);
                let (result, relaunch) = match fs::rename(&backup, current) {
                    Ok(()) => {
                        let _ = fs::remove_dir_all(stage);
                        (Err(error), true)
                    }
                    Err(restore) => (
                        Err(format!("{error}; restoring the previous app: {restore}")),
                        false,
                    ),
                };
                // The outcome lands before the old app does: the relaunched GUI
                // merges it at startup, and a write afterward races that merge
                // (#540).
                report(&result);
                if relaunch {
                    let _ = launch(current);
                }
                result
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
        // The updater is spawned directly by the app it waits for, so while
        // that app lives it *is* this process's parent, and the kernel
        // reparents us to launchd the moment it exits. Watching getppid() is
        // therefore immune to pid reuse, which `kill(pid, 0)` is not: a
        // recycled pid keeps answering 0 forever. (Windows solves the same
        // race by holding a process handle — see the windows module.)
        let pid = pid as libc::pid_t;
        if unsafe { libc::getppid() } == pid {
            while unsafe { libc::getppid() } == pid {
                thread::sleep(PARENT_POLL);
            }
            return;
        }
        // Not our parent — a hand-run updater. The polling fallback keeps
        // that invocation working, pid-reuse caveat and all.
        while process_alive(pid) {
            thread::sleep(PARENT_POLL);
        }
    }

    fn process_alive(pid: libc::pid_t) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
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

            replace_and_relaunch(&current, &replacement, &stage, |_| Ok(()), |_| ()).unwrap();

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
            let reported_after_launches = std::cell::Cell::new(usize::MAX);

            let error = replace_and_relaunch(
                &current,
                &replacement,
                &stage,
                |_| {
                    launches.set(launches.get() + 1);
                    if launches.get() == 1 {
                        Err("new app failed".to_string())
                    } else {
                        Ok(())
                    }
                },
                |_| reported_after_launches.set(launches.get()),
            )
            .unwrap_err();

            assert_eq!(error, "new app failed");
            assert_eq!(launches.get(), 2);
            // The outcome is reported after the failed first launch but before
            // the old app comes back — the relaunched GUI must find it already
            // on disk at startup (#540).
            assert_eq!(reported_after_launches.get(), 1);
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

            replace_and_relaunch(&current, &replacement, &stage, |_| Ok(()), |_| ()).unwrap();

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

        #[test]
        fn bundle_version_preserves_the_complete_nightly_identity() {
            let root = tempfile::tempdir().unwrap();
            let app = root.path().join("tty7.app");
            let contents = app.join("Contents");
            fs::create_dir_all(&contents).unwrap();
            fs::write(
                contents.join("Info.plist"),
                r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
    <key>CFBundleShortVersionString</key>
    <string>26.8.2-nightly.20260803</string>
</dict>
</plist>
"#,
            )
            .unwrap();

            assert_eq!(bundle_version(&app).unwrap(), "26.8.2-nightly.20260803");
        }
    }
}

/// The Linux half serves exactly one installation shape: an AppImage. The
/// installed artifact is a single file (the path `$APPIMAGE` names), so the
/// whole install is one atomic swap — move the running image aside, rename
/// the verified download into its place, and start it. Tarball and distro
/// installs never reach this program; `package_for_current_install` in
/// `core::update` hands them the release page instead.
///
/// One Linux-specific constraint shapes the code: the image the GUI runs
/// from is a FUSE mount the AppImage runtime tears down when the app exits —
/// which is the moment `install` starts working. The GUI therefore copies
/// this helper out of the mount into the staging directory and runs the
/// copy, the same way the Windows path runs a private copy because Setup
/// replaces the installed one.
#[cfg(target_os = "linux")]
mod linux {
    use std::fs::{self, OpenOptions};
    use std::io::{Read as _, Write as _};
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::thread;
    use std::time::Duration;

    const PARENT_POLL: Duration = Duration::from_millis(100);
    const LAUNCH_GRACE: Duration = Duration::from_secs(1);

    /// Where `bundle-appimage.sh` installs the desktop entry inside the
    /// image. The root-level `tty7.desktop` is linuxdeploy's symlink to this
    /// file, and extracting a symlink alone yields a dangling link — so the
    /// real path is the one asked for.
    const DESKTOP_ENTRY: &str = "usr/share/applications/tty7.desktop";
    /// The helper inside the image, beside the app the way every platform
    /// ships it.
    const BUNDLED_UPDATER: &str = "usr/bin/tty7-updater";
    /// The desktop-entry key `bundle-appimage.sh` stamps the release version
    /// into — the AppImage convention for stating a version where tools can
    /// read it without running the app.
    const VERSION_KEY: &str = "X-AppImage-Version=";

    pub fn run() -> Result<(), String> {
        let mut args = std::env::args_os().skip(1);
        let command = args
            .next()
            .and_then(|arg| arg.into_string().ok())
            .ok_or_else(usage)?;
        match command.as_str() {
            "verify" => {
                let archive = next_path(&mut args)?;
                let checksums = next_path(&mut args)?;
                let asset_name = next_string(&mut args)?;
                let stage = next_path(&mut args)?;
                let expected_version = next_string(&mut args)?;
                reject_extra(args)?;
                verify_archive(&archive, &checksums, &asset_name)?;
                verify_update(&archive, &stage, &expected_version)
            }
            "install" => {
                let parent_pid = next_string(&mut args)?
                    .parse::<u32>()
                    .map_err(|_| "parent pid is not an unsigned integer".to_string())?;
                let current = next_path(&mut args)?;
                let archive = next_path(&mut args)?;
                let checksums = next_path(&mut args)?;
                let asset_name = next_string(&mut args)?;
                let stage = next_path(&mut args)?;
                let expected_version = next_string(&mut args)?;
                let log = next_path(&mut args)?;
                let options = tail_options(args)?;
                options.apply();
                install(InstallPlan {
                    parent_pid,
                    current,
                    archive,
                    checksums,
                    asset_name,
                    stage,
                    expected_version,
                    log,
                    result_file: options.result_file,
                })
            }
            _ => Err(usage()),
        }
    }

    fn usage() -> String {
        "usage: tty7-updater verify <archive.AppImage> <checksums.txt> \
         <asset-name> <stage-dir> <version>\n\
         or: tty7-updater install <parent-pid> <current.AppImage> <archive.AppImage> \
         <checksums.txt> <asset-name> <stage-dir> <version> <log-path> \
         [--config-dir <dir>] [--result-file <path>]"
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

    /// The named options an install verb takes after its positional
    /// arguments. See the Windows half of this file for why these are
    /// arguments and not the environment.
    #[derive(Default)]
    struct TailOptions {
        config_dir: Option<PathBuf>,
        result_file: Option<PathBuf>,
    }

    fn tail_options(
        mut args: impl Iterator<Item = std::ffi::OsString>,
    ) -> Result<TailOptions, String> {
        let mut options = TailOptions::default();
        while let Some(arg) = args.next() {
            match arg.to_str() {
                Some("--config-dir") => options.config_dir = Some(next_path(&mut args)?),
                Some("--result-file") => options.result_file = Some(next_path(&mut args)?),
                _ => return Err(usage()),
            }
        }
        Ok(options)
    }

    impl TailOptions {
        fn apply(&self) {
            let Some(dir) = &self.config_dir else { return };
            tty7_core::core::config::set_config_dir(dir.clone());
            // Re-exported so the relaunched app — a child of this process —
            // keeps answering for the same config directory. Safe here:
            // argument parsing runs before any thread exists.
            unsafe { std::env::set_var("TTY7_CONFIG_DIR", dir) };
        }
    }

    /// The terminal outcome of the attempt, for the next GUI launch to merge
    /// into the update state (#540). Best-effort, like every log line here.
    fn report_outcome(
        result_file: Option<&Path>,
        log: &Path,
        version: &str,
        result: &Result<(), String>,
    ) {
        let Some(path) = result_file else { return };
        let outcome = tty7_core::daemon::install::outcome::UpdateOutcome {
            version: version.to_string(),
            ok: result.is_ok(),
            detail: result.as_ref().err().cloned(),
        };
        if let Err(error) = tty7_core::daemon::install::outcome::write_outcome(path, &outcome) {
            log_line(
                log,
                &format!(
                    "could not record the update outcome at {}: {error}",
                    path.display()
                ),
            );
        }
    }

    struct InstallPlan {
        parent_pid: u32,
        current: PathBuf,
        archive: PathBuf,
        checksums: PathBuf,
        asset_name: String,
        stage: PathBuf,
        expected_version: String,
        log: PathBuf,
        result_file: Option<PathBuf>,
    }

    fn install(plan: InstallPlan) -> Result<(), String> {
        install_inner(&plan)
    }

    // The daemon is deliberately left running, exactly as on macOS: nothing
    // locks a running executable's file on Linux, and the daemon serves its
    // panes from the old mount until the user chooses to restart it — that
    // is what keeps their shells alive across the update.
    fn install_inner(plan: &InstallPlan) -> Result<(), String> {
        wait_for_exit(plan.parent_pid);
        log_line(&plan.log, "re-verifying the staged tty7 update");
        let verification = verify_archive(&plan.archive, &plan.checksums, &plan.asset_name)
            .and_then(|()| verify_update(&plan.archive, &plan.stage, &plan.expected_version));
        if let Err(error) = verification {
            log_line(&plan.log, &error);
            let _ = fs::remove_dir_all(&plan.stage);
            let result = Err(error);
            // The outcome lands before the old app does: the relaunched GUI
            // merges it at startup, and a write afterward races that merge
            // (#540).
            report_outcome(
                plan.result_file.as_deref(),
                &plan.log,
                &plan.expected_version,
                &result,
            );
            let _ = launch_app(&plan.current);
            return result;
        }
        log_line(&plan.log, &format!("replacing {}", plan.current.display()));
        let report = |result: &Result<(), String>| {
            report_outcome(
                plan.result_file.as_deref(),
                &plan.log,
                &plan.expected_version,
                result,
            );
        };
        replace_and_relaunch(
            &plan.current,
            &plan.archive,
            &plan.stage,
            launch_app,
            report,
        )
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

    /// What the downloaded file has to prove before it may become the
    /// installation: it is a type-2 AppImage at all, it states the version
    /// this update was for, and it carries its own updater — an image
    /// without one would install fine and then be the last version that
    /// ever could. Runs only after `verify_archive` has pinned the bytes to
    /// the release's checksums.txt; from there, running the image's own
    /// `--appimage-extract` is running the released code, which is exactly
    /// what the swap is about to do anyway.
    fn verify_update(staged: &Path, stage: &Path, expected_version: &str) -> Result<(), String> {
        if !is_appimage(&read_header(staged)?) {
            return Err(format!("{} is not a type-2 AppImage", staged.display()));
        }
        // Downloaded bytes land without the execute bit; extraction needs the
        // runtime to run. The definitive mode is set again at swap time, taken
        // from the file being replaced.
        make_executable(staged)?;
        let desktop = extract_entry(staged, stage, DESKTOP_ENTRY)?;
        let text = fs::read_to_string(&desktop)
            .map_err(|error| format!("reading {}: {error}", desktop.display()))?;
        let actual = version_from_desktop_entry(&text).ok_or_else(|| {
            format!("the staged AppImage's desktop entry carries no {VERSION_KEY}")
        })?;
        if actual != expected_version {
            return Err(format!(
                "the staged AppImage reports version {actual}, expected {expected_version}"
            ));
        }
        extract_entry(staged, stage, BUNDLED_UPDATER)?;
        Ok(())
    }

    /// ELF with the AppImage type-2 marker (`AI\x02` at offset 8). The
    /// runtime the swap is about to spawn only exists behind this shape; a
    /// wrongly published asset — a tarball under the AppImage name, an HTML
    /// error page — fails here with a name instead of at launch.
    fn is_appimage(header: &[u8]) -> bool {
        header.len() >= 11
            && header[..4] == [0x7f, b'E', b'L', b'F']
            && header[8..11] == [b'A', b'I', 0x02]
    }

    fn read_header(path: &Path) -> Result<Vec<u8>, String> {
        let mut file =
            fs::File::open(path).map_err(|error| format!("reading {}: {error}", path.display()))?;
        let mut header = [0u8; 16];
        let read = file
            .read(&mut header)
            .map_err(|error| format!("reading {}: {error}", path.display()))?;
        Ok(header[..read].to_vec())
    }

    fn make_executable(path: &Path) -> Result<(), String> {
        let mode = fs::metadata(path)
            .map_err(|error| format!("reading the mode of {}: {error}", path.display()))?
            .permissions()
            .mode();
        fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o755))
            .map_err(|error| format!("marking {} executable: {error}", path.display()))
    }

    /// Unpacks one entry of the staged image into `<stage>/squashfs-root/`
    /// and returns the extracted file's path.
    ///
    /// `--appimage-extract` is answered by the AppImage runtime itself,
    /// before any application code, and unpacks without mounting — so it
    /// works on machines whose FUSE setup the eventual launch will need but
    /// this verification should not. Run from the staging directory so the
    /// `squashfs-root` it creates lands beside the package and is removed
    /// with it. The runtime exits zero even when nothing matched, which is
    /// why the answer is the extracted file's existence rather than the
    /// exit status.
    fn extract_entry(appimage: &Path, stage: &Path, entry: &str) -> Result<PathBuf, String> {
        // Anchored before the spawn: exec resolves a relative program path
        // against the child's working directory, which the line below moves —
        // a hand-run `tty7-updater verify ./pkg.AppImage …` would otherwise
        // fail with a bare "No such file or directory".
        let appimage = std::path::absolute(appimage)
            .map_err(|error| format!("resolving {}: {error}", appimage.display()))?;
        run_checked(
            Command::new(&appimage)
                .args(["--appimage-extract", entry])
                .current_dir(stage)
                .stdout(Stdio::null()),
            "extracting from the staged AppImage",
        )?;
        let extracted = stage.join("squashfs-root").join(entry);
        if !extracted.is_file() {
            return Err(format!("the staged AppImage carries no {entry}"));
        }
        Ok(extracted)
    }

    fn version_from_desktop_entry(text: &str) -> Option<String> {
        text.lines()
            .find_map(|line| line.strip_prefix(VERSION_KEY))
            .map(|version| version.trim().to_string())
            .filter(|version| !version.is_empty())
    }

    fn replace_and_relaunch(
        current: &Path,
        replacement: &Path,
        stage: &Path,
        launch: impl Fn(&Path) -> Result<(), String>,
        report: impl Fn(&Result<(), String>),
    ) -> Result<(), String> {
        // The staging directory is a fresh TempDir created beside the current
        // image, so a backup here stays on the same filesystem without using a
        // predictable sibling path. In particular, never delete a fixed-name
        // path beside the image: it may be a recovery copy left by an
        // interrupted update (or simply an unrelated user-owned path).
        let backup = stage.join("previous.AppImage");
        if backup.exists() {
            let result = Err(format!(
                "the update staging backup already exists: {}",
                backup.display()
            ));
            report(&result);
            return result;
        }
        // The replacement wears the current image's own mode: a rename keeps
        // the staged file's permissions, which are the download's, and the
        // user's choice of who may run their tty7 is not this program's to
        // revise. Owner execute is guaranteed on top — without it nothing can
        // relaunch — and grants nobody else anything.
        if let Err(error) = carry_mode(current, replacement) {
            report(&Err(error.clone()));
            return Err(error);
        }
        if let Err(error) = fs::rename(current, &backup) {
            let result = Err(format!("moving the current AppImage aside: {error}"));
            report(&result);
            return result;
        }

        if let Err(error) = fs::rename(replacement, current) {
            let _ = fs::rename(&backup, current);
            let _ = fs::remove_dir_all(stage);
            let result = Err(format!("putting the staged AppImage in place: {error}"));
            report(&result);
            return result;
        }

        match launch(current) {
            Ok(()) => {
                let _ = remove_path(&backup);
                let _ = fs::remove_dir_all(stage);
                let result = Ok(());
                report(&result);
                result
            }
            Err(error) => {
                let _ = remove_path(current);
                let (result, relaunch) = match fs::rename(&backup, current) {
                    Ok(()) => {
                        let _ = fs::remove_dir_all(stage);
                        (Err(error), true)
                    }
                    Err(restore) => (
                        Err(format!("{error}; restoring the previous image: {restore}")),
                        false,
                    ),
                };
                // The outcome lands before the old app does: the relaunched
                // GUI merges it at startup, and a write afterward races that
                // merge (#540).
                report(&result);
                if relaunch {
                    let _ = launch(current);
                }
                result
            }
        }
    }

    /// Puts the mode of the file being replaced onto its replacement,
    /// with owner execute assured. Falls back to plain 0o755 when the
    /// current image cannot answer — it is about to be renamed away, not
    /// consulted as an authority.
    fn carry_mode(current: &Path, replacement: &Path) -> Result<(), String> {
        let mode = fs::metadata(current)
            .map(|meta| meta.permissions().mode())
            .unwrap_or(0o755);
        fs::set_permissions(replacement, fs::Permissions::from_mode(mode | 0o700))
            .map_err(|error| format!("setting the mode of {}: {error}", replacement.display()))
    }

    /// Starts the image at its installed path. The runtime sets `$APPIMAGE`
    /// and `$APPDIR` for the process it mounts, overwriting the stale pair
    /// this process inherited from the app that spawned it.
    fn launch_app(appimage: &Path) -> Result<(), String> {
        let mut child = Command::new(appimage)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("launching {}: {error}", appimage.display()))?;
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
        // The updater is spawned directly by the app it waits for, so while
        // that app lives it *is* this process's parent, and the kernel
        // reparents us the moment it exits. Watching getppid() is therefore
        // immune to pid reuse, which `kill(pid, 0)` is not: a recycled pid
        // keeps answering 0 forever. Same reasoning as the macos module;
        // Linux reparents to init or the nearest subreaper, and either way
        // the answer stops being `pid`.
        let pid = pid as libc::pid_t;
        if unsafe { libc::getppid() } == pid {
            while unsafe { libc::getppid() } == pid {
                thread::sleep(PARENT_POLL);
            }
            return;
        }
        // Not our parent — a hand-run updater. The polling fallback keeps
        // that invocation working, pid-reuse caveat and all.
        while process_alive(pid) {
            thread::sleep(PARENT_POLL);
        }
    }

    fn process_alive(pid: libc::pid_t) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
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

        #[test]
        fn successful_launch_commits_the_replacement() {
            let root = tempfile::tempdir().unwrap();
            let current = root.path().join("tty7.AppImage");
            let stage = root.path().join("stage");
            fs::create_dir(&stage).unwrap();
            let replacement = stage.join("tty7-new.AppImage");
            fs::write(&current, b"old image").unwrap();
            fs::write(&replacement, b"new image").unwrap();

            replace_and_relaunch(&current, &replacement, &stage, |_| Ok(()), |_| ()).unwrap();

            assert_eq!(fs::read(&current).unwrap(), b"new image");
            assert!(!stage.exists());
        }

        #[test]
        fn failed_launch_restores_and_relaunches_the_previous_image() {
            let root = tempfile::tempdir().unwrap();
            let current = root.path().join("tty7.AppImage");
            let stage = root.path().join("stage");
            fs::create_dir(&stage).unwrap();
            let replacement = stage.join("tty7-new.AppImage");
            fs::write(&current, b"old image").unwrap();
            fs::write(&replacement, b"new image").unwrap();
            let launches = std::cell::Cell::new(0);
            let reported_after_launches = std::cell::Cell::new(usize::MAX);

            let error = replace_and_relaunch(
                &current,
                &replacement,
                &stage,
                |_| {
                    launches.set(launches.get() + 1);
                    if launches.get() == 1 {
                        Err("new app failed".to_string())
                    } else {
                        Ok(())
                    }
                },
                |_| reported_after_launches.set(launches.get()),
            )
            .unwrap_err();

            assert_eq!(error, "new app failed");
            assert_eq!(launches.get(), 2);
            // The outcome is reported after the failed first launch but before
            // the old app comes back — the relaunched GUI must find it already
            // on disk at startup (#540).
            assert_eq!(reported_after_launches.get(), 1);
            assert_eq!(fs::read(&current).unwrap(), b"old image");
            assert!(!stage.exists());
        }

        /// The installed image keeps the mode the user gave it — a 0700
        /// image stays private — while the download's missing execute bit
        /// never survives into the installation.
        #[test]
        fn the_replacement_wears_the_current_images_mode() {
            let root = tempfile::tempdir().unwrap();
            let current = root.path().join("tty7.AppImage");
            let stage = root.path().join("stage");
            fs::create_dir(&stage).unwrap();
            let replacement = stage.join("tty7-new.AppImage");
            fs::write(&current, b"old image").unwrap();
            fs::write(&replacement, b"new image").unwrap();
            fs::set_permissions(&current, fs::Permissions::from_mode(0o700)).unwrap();
            fs::set_permissions(&replacement, fs::Permissions::from_mode(0o644)).unwrap();

            replace_and_relaunch(&current, &replacement, &stage, |_| Ok(()), |_| ()).unwrap();

            let mode = fs::metadata(&current).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "mode {mode:o}");
        }

        /// A leftover backup means an earlier attempt stopped between its two
        /// renames; installing over it would overwrite the one copy of the
        /// previous version.
        #[test]
        fn an_existing_backup_stops_the_replacement() {
            let root = tempfile::tempdir().unwrap();
            let current = root.path().join("tty7.AppImage");
            let stage = root.path().join("stage");
            fs::create_dir(&stage).unwrap();
            let replacement = stage.join("tty7-new.AppImage");
            fs::write(&current, b"old image").unwrap();
            fs::write(&replacement, b"new image").unwrap();
            fs::write(stage.join("previous.AppImage"), b"earlier backup").unwrap();

            let error = replace_and_relaunch(
                &current,
                &replacement,
                &stage,
                |_| panic!("nothing may launch when the backup path is taken"),
                |_| (),
            )
            .unwrap_err();

            assert!(error.contains("backup already exists"), "{error}");
            assert_eq!(fs::read(&current).unwrap(), b"old image");
        }

        #[test]
        fn archive_verification_rejects_bytes_that_do_not_match_the_manifest() {
            let root = tempfile::tempdir().unwrap();
            let archive = root.path().join("tty7.AppImage");
            let manifest = root.path().join("checksums.txt");
            fs::write(&archive, b"downloaded bytes").unwrap();
            fs::write(
                &manifest,
                format!(
                    "{}  tty7.AppImage\n",
                    tty7_core::daemon::install::checksums::hex(
                        &tty7_core::daemon::install::checksums::sha256(b"published bytes")
                    )
                ),
            )
            .unwrap();

            let error = verify_archive(&archive, &manifest, "tty7.AppImage").unwrap_err();
            assert!(error.contains("failed sha256 verification"), "{error}");
        }

        /// The magic check runs before anything executes the download, so a
        /// mis-published asset is named without being run.
        #[test]
        fn verification_rejects_a_file_that_is_not_an_appimage() {
            let mut elf_with_marker = vec![0x7f, b'E', b'L', b'F', 2, 1, 1, 0, b'A', b'I', 0x02];
            elf_with_marker.resize(16, 0);
            assert!(is_appimage(&elf_with_marker));
            // A plain ELF — the tarball's binary, say — is not an AppImage.
            let mut bare_elf = vec![0x7f, b'E', b'L', b'F', 2, 1, 1, 0, 0, 0, 0];
            bare_elf.resize(16, 0);
            assert!(!is_appimage(&bare_elf));
            assert!(!is_appimage(b"<html>Not Found</html>"));
            assert!(!is_appimage(b""));
            assert!(!is_appimage(&[0x7f, b'E', b'L', b'F']));

            let root = tempfile::tempdir().unwrap();
            let staged = root.path().join("tty7.AppImage");
            fs::write(&staged, b"<html>Not Found</html>").unwrap();
            let error = verify_update(&staged, root.path(), "27.1.0").unwrap_err();
            assert!(error.contains("not a type-2 AppImage"), "{error}");
        }

        #[test]
        fn the_desktop_entry_states_the_version() {
            let text = "[Desktop Entry]\nType=Application\nName=tty7\nExec=tty7-app\n\
                        Icon=tty7\nX-AppImage-Version=26.8.4\n";
            assert_eq!(version_from_desktop_entry(text).as_deref(), Some("26.8.4"));
            // The nightly stamp survives whole — the identity the GUI
            // compares against carries the prerelease tail.
            assert_eq!(
                version_from_desktop_entry("X-AppImage-Version=26.8.4-nightly.202608140200\n")
                    .as_deref(),
                Some("26.8.4-nightly.202608140200")
            );
            assert_eq!(
                version_from_desktop_entry("[Desktop Entry]\nName=tty7\n"),
                None
            );
            // A stated nothing is not a version.
            assert_eq!(version_from_desktop_entry("X-AppImage-Version=\n"), None);
            assert_eq!(version_from_desktop_entry("X-AppImage-Version=  \n"), None);
        }
    }
}

#[cfg(target_os = "windows")]
mod windows {
    use std::collections::HashSet;
    use std::ffi::{OsStr, OsString, c_void};
    use std::fs::{self, OpenOptions};
    use std::io::Write as _;
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt as _;
    use std::path::{Component, Path, PathBuf};
    use std::process::{Child, Command, ExitStatus, Stdio};
    use std::ptr::null_mut;
    use std::thread;
    use std::time::{Duration, Instant};

    use smol::io::AsyncReadExt as _;

    use tty7_core::daemon::install::outcome::UpdateOutcome;
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, GetLastError, HANDLE, LocalFree,
        WAIT_FAILED, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateDirectoryW, GetFileVersionInfoSizeW, GetFileVersionInfoW, VS_FIXEDFILEINFO,
        VerQueryValueW,
    };
    use windows_sys::Win32::System::Threading::{
        INFINITE, OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };

    const LAUNCH_GRACE: Duration = Duration::from_secs(1);
    const PORTABLE_PAYLOAD_DIR: &str = "portable-payload";
    const PORTABLE_MARKER: &str = ".tty7-portable";
    const PORTABLE_MARKER_CONTENT: &[u8] = b"portable-v1";
    /// Lives inside a portable-update backup from before the first installed
    /// file moves until the replacement is fully in place. A backup found
    /// later still carrying it names a replacement that was cut short —
    /// power loss, a kill — and an installation that may mix two versions;
    /// one without it is a finished update whose backup deletion lost to an
    /// antivirus scan. The app reads it at launch: duplicated in
    /// src/core/update.rs, like the portable marker above.
    const PORTABLE_BACKUP_INCOMPLETE: &str = ".tty7-replace-incomplete";
    const MAX_PORTABLE_ENTRIES: usize = 4096;
    const MAX_PORTABLE_EXPANDED_BYTES: u64 = 1024 * 1024 * 1024;
    // Everything the release package owns: an entry outside this list is
    // rejected on extraction, and everything in it is moved aside and replaced
    // during an in-place portable update. A file the package ships but this
    // list omits would survive forever at its installed version.
    const PORTABLE_MANAGED_ROOTS: [&str; 11] = [
        "tty7-app.exe",
        "tty7.exe",
        "tty7-updater.exe",
        PORTABLE_MARKER,
        "completions",
        "server",
        "LICENSE.txt",
        "README.md",
        // The bundled ConPTY pair and its notice. Deliberately absent from
        // `verify_portable_payload`: tty7 runs without them, falling back to
        // the in-box conhost, so a package that forgot them is a release bug
        // to catch in CI (verify-windows-package.ps1) rather than a reason to
        // abort a user's update after the download.
        "conpty.dll",
        "OpenConsole.exe",
        "LICENSE-ConPTY.txt",
    ];

    pub fn run() -> Result<(), String> {
        let mut args = std::env::args_os().skip(1);
        let command = args
            .next()
            .and_then(|arg| arg.into_string().ok())
            .ok_or_else(usage)?;
        match command.as_str() {
            "verify" => {
                let installer = next_path(&mut args)?;
                let checksums = next_path(&mut args)?;
                let asset_name = next_string(&mut args)?;
                let expected_version = next_string(&mut args)?;
                reject_extra(args)?;
                verify_update(&installer, &checksums, &asset_name, &expected_version)
            }
            "verify-portable" => {
                let archive = next_path(&mut args)?;
                let checksums = next_path(&mut args)?;
                let asset_name = next_string(&mut args)?;
                let expected_version = next_string(&mut args)?;
                let stage = next_path(&mut args)?;
                reject_extra(args)?;
                verify_portable_update(
                    &archive,
                    &checksums,
                    &asset_name,
                    &expected_version,
                    &stage.join(PORTABLE_PAYLOAD_DIR),
                )
            }
            "install" => {
                let parent_pid = next_string(&mut args)?
                    .parse::<u32>()
                    .map_err(|_| "parent pid is not an unsigned integer".to_string())?;
                let installer = next_path(&mut args)?;
                let checksums = next_path(&mut args)?;
                let asset_name = next_string(&mut args)?;
                let install_dir = next_path(&mut args)?;
                let expected_version = next_string(&mut args)?;
                let log = next_path(&mut args)?;
                let stage = next_path(&mut args)?;
                let options = tail_options(args)?;
                options.apply();
                install(InstallPlan {
                    parent_pid,
                    installer,
                    checksums,
                    asset_name,
                    install_dir,
                    expected_version,
                    log,
                    stage,
                    result_file: options.result_file,
                    expected_sha256: None,
                })
            }
            "install-portable" => {
                let parent_pid = next_string(&mut args)?
                    .parse::<u32>()
                    .map_err(|_| "parent pid is not an unsigned integer".to_string())?;
                let archive = next_path(&mut args)?;
                let checksums = next_path(&mut args)?;
                let asset_name = next_string(&mut args)?;
                let install_dir = next_path(&mut args)?;
                let expected_version = next_string(&mut args)?;
                let log = next_path(&mut args)?;
                let stage = next_path(&mut args)?;
                let options = tail_options(args)?;
                options.apply();
                install_portable(PortableInstallPlan {
                    parent_pid,
                    archive,
                    checksums,
                    asset_name,
                    install_dir,
                    expected_version,
                    log,
                    stage,
                    result_file: options.result_file,
                })
            }
            "cleanup" => {
                let parent_pid = next_string(&mut args)?
                    .parse::<u32>()
                    .map_err(|_| "parent pid is not an unsigned integer".to_string())?;
                let stage = next_path(&mut args)?;
                reject_extra(args)?;
                wait_for_exit(parent_pid)?;
                fs::remove_dir_all(&stage)
                    .map_err(|error| format!("removing {}: {error}", stage.display()))
            }
            "capabilities" => {
                reject_extra(args)?;
                // One token per line. The GUI reads this to learn whether the
                // *installed* updater — the binary a UAC prompt would point
                // at — speaks the elevation verbs; a build that predates them
                // exits with the usage error above instead, which is the same
                // answer (#504).
                for capability in ELEVATION_CAPABILITIES {
                    println!("{capability}");
                }
                Ok(())
            }
            "elevated-stage" => {
                let gui_pid = next_string(&mut args)?
                    .parse::<u32>()
                    .map_err(|_| "gui pid is not an unsigned integer".to_string())?;
                let installer = next_path(&mut args)?;
                let asset_name = next_string(&mut args)?;
                let install_dir = next_path(&mut args)?;
                let expected_version = next_string(&mut args)?;
                let log = next_path(&mut args)?;
                let stage = next_path(&mut args)?;
                let options = tail_options(args)?;
                options.apply();
                let (Some(expected_sha256), Some(status_file)) =
                    (options.expected_sha256, options.status_file)
                else {
                    return Err(usage());
                };
                elevated_stage(ElevatedStagePlan {
                    gui_pid,
                    installer,
                    asset_name,
                    install_dir,
                    expected_version,
                    log,
                    stage,
                    expected_sha256,
                    status_file,
                    result_file: options.result_file,
                    config_dir: options.config_dir,
                })
            }
            "install-elevated" => {
                let parent_pid = next_string(&mut args)?
                    .parse::<u32>()
                    .map_err(|_| "parent pid is not an unsigned integer".to_string())?;
                let installer = next_path(&mut args)?;
                let checksums = next_path(&mut args)?;
                let asset_name = next_string(&mut args)?;
                let install_dir = next_path(&mut args)?;
                let expected_version = next_string(&mut args)?;
                let log = next_path(&mut args)?;
                let stage = next_path(&mut args)?;
                let options = tail_options(args)?;
                options.apply();
                let Some(expected_sha256) = options.expected_sha256 else {
                    return Err(usage());
                };
                install_elevated(InstallPlan {
                    parent_pid,
                    installer,
                    checksums,
                    asset_name,
                    install_dir,
                    expected_version,
                    log,
                    stage,
                    result_file: options.result_file,
                    expected_sha256: Some(expected_sha256),
                })
            }
            "relaunch-watcher" => {
                let options = tail_options(args)?;
                options.apply();
                watch(&WatcherPlan {
                    status_file: options.status_file.ok_or_else(usage)?,
                    result_file: options.result_file.ok_or_else(usage)?,
                    app: options.app_path.ok_or_else(usage)?,
                    log: options.log.ok_or_else(usage)?,
                    version: options.expected_version.ok_or_else(usage)?,
                    gui_pid: options.gui_pid,
                })
            }
            _ => Err(usage()),
        }
    }

    fn usage() -> String {
        "usage: tty7-updater verify <setup.exe> <checksums.txt> <asset-name> <version>\n\
         or: tty7-updater install <parent-pid> <setup.exe> <checksums.txt> <asset-name> \
         <install-dir> <version> <log-path> <stage-dir> \
         [--config-dir <dir>] [--result-file <path>]\n\
         or: tty7-updater verify-portable <archive.zip> <checksums.txt> <asset-name> \
         <version> <stage-dir>\n\
         or: tty7-updater install-portable <parent-pid> <archive.zip> <checksums.txt> \
         <asset-name> <install-dir> <version> <log-path> <stage-dir> \
         [--config-dir <dir>] [--result-file <path>]\n\
         or: tty7-updater cleanup <parent-pid> <stage-dir>\n\
         or: tty7-updater capabilities\n\
         or: tty7-updater elevated-stage <gui-pid> <setup.exe> <asset-name> \
         <install-dir> <version> <log-path> <stage-dir> \
         --expected-sha256 <hex> --status-file <path> \
         [--config-dir <dir>] [--result-file <path>]\n\
         or: tty7-updater install-elevated <parent-pid> <setup.exe> <checksums.txt> \
         <asset-name> <install-dir> <version> <log-path> <stage-dir> \
         --expected-sha256 <hex> [--config-dir <dir>] [--result-file <path>]\n\
         or: tty7-updater relaunch-watcher --status-file <path> --result-file <path> \
         --app-path <tty7-app.exe> --log <path> --expected-version <version> \
         [--gui-pid <pid>] [--config-dir <dir>]"
            .to_string()
    }

    fn next_path(args: &mut impl Iterator<Item = OsString>) -> Result<PathBuf, String> {
        args.next().map(PathBuf::from).ok_or_else(usage)
    }

    fn next_string(args: &mut impl Iterator<Item = OsString>) -> Result<String, String> {
        args.next()
            .and_then(|arg| arg.into_string().ok())
            .ok_or_else(usage)
    }

    fn reject_extra(mut args: impl Iterator<Item = OsString>) -> Result<(), String> {
        if args.next().is_some() {
            Err(usage())
        } else {
            Ok(())
        }
    }

    /// The named options an install verb takes after its positional arguments.
    ///
    /// Everything the caller needs this process to know travels this way —
    /// never through the environment. An elevated (UAC) child does not
    /// inherit the spawning GUI's environment, so a `TTY7_CONFIG_DIR` set
    /// there would silently fall back to the *administrator's* config
    /// directory under an over-the-shoulder elevation (#504).
    #[derive(Default)]
    struct TailOptions {
        config_dir: Option<PathBuf>,
        result_file: Option<PathBuf>,
        /// The staged package's digest as the release server published it.
        /// Crosses the elevation boundary on the command line because the
        /// checksums file beside the package cannot anchor trust there: a
        /// medium-integrity process can rewrite both together.
        expected_sha256: Option<String>,
        /// Where `elevated-stage` tells the watcher which pid names the
        /// install chain.
        status_file: Option<PathBuf>,
        /// The `tty7-app.exe` the watcher relaunches.
        app_path: Option<PathBuf>,
        /// Log override for the verbs that take no positional log path (the
        /// watcher).
        log: Option<PathBuf>,
        /// The version being installed, for the watcher's synthesized
        /// outcomes.
        expected_version: Option<String>,
        /// The GUI the watcher must not relaunch over. See [`WatcherPlan`].
        gui_pid: Option<u32>,
    }

    fn tail_options(mut args: impl Iterator<Item = OsString>) -> Result<TailOptions, String> {
        let mut options = TailOptions::default();
        while let Some(arg) = args.next() {
            match arg.to_str() {
                Some("--config-dir") => options.config_dir = Some(next_path(&mut args)?),
                Some("--result-file") => options.result_file = Some(next_path(&mut args)?),
                Some("--expected-sha256") => {
                    options.expected_sha256 = Some(next_string(&mut args)?)
                }
                Some("--status-file") => options.status_file = Some(next_path(&mut args)?),
                Some("--app-path") => options.app_path = Some(next_path(&mut args)?),
                Some("--log") => options.log = Some(next_path(&mut args)?),
                Some("--expected-version") => {
                    options.expected_version = Some(next_string(&mut args)?)
                }
                Some("--gui-pid") => {
                    options.gui_pid = Some(
                        next_string(&mut args)?
                            .parse::<u32>()
                            .map_err(|_| "gui pid is not an unsigned integer".to_string())?,
                    )
                }
                _ => return Err(usage()),
            }
        }
        Ok(options)
    }

    impl TailOptions {
        fn apply(&self) {
            let Some(dir) = &self.config_dir else { return };
            tty7_core::core::config::set_config_dir(dir.clone());
            // The override above covers this process; the variable is
            // re-exported so the helper children this process spawns — the
            // payload's `--stop-daemon`, the relaunched app — keep answering
            // for the same config directory. Safe here: argument parsing runs
            // before any thread exists.
            unsafe { std::env::set_var("TTY7_CONFIG_DIR", dir) };
        }
    }

    /// The terminal outcome of the attempt, for the next GUI launch to merge
    /// into the update state (#540). Best-effort: a result that cannot be
    /// recorded goes to the log like every other updater detail. On paths that
    /// relaunch the previous app this runs *before* the relaunch, so the GUI
    /// finds the outcome already on disk when it starts.
    fn report_outcome(
        result_file: Option<&Path>,
        log: &Path,
        version: &str,
        result: &Result<(), String>,
    ) {
        let Some(path) = result_file else { return };
        let outcome = tty7_core::daemon::install::outcome::UpdateOutcome {
            version: version.to_string(),
            ok: result.is_ok(),
            detail: result.as_ref().err().cloned(),
        };
        if let Err(error) = tty7_core::daemon::install::outcome::write_outcome(path, &outcome) {
            log_line(
                log,
                &format!(
                    "could not record the update outcome at {}: {error}",
                    path.display()
                ),
            );
        }
    }

    struct InstallPlan {
        parent_pid: u32,
        installer: PathBuf,
        checksums: PathBuf,
        asset_name: String,
        install_dir: PathBuf,
        expected_version: String,
        log: PathBuf,
        stage: PathBuf,
        result_file: Option<PathBuf>,
        /// Set on the elevated path, where the digest — not the checksums
        /// file it was copied with — is the trust anchor (#504).
        expected_sha256: Option<String>,
    }

    struct PortableInstallPlan {
        parent_pid: u32,
        archive: PathBuf,
        checksums: PathBuf,
        asset_name: String,
        install_dir: PathBuf,
        expected_version: String,
        log: PathBuf,
        stage: PathBuf,
        result_file: Option<PathBuf>,
    }

    /// How an install ends. The distinction exists because an elevated
    /// process must never spawn `tty7-app.exe`: the app it launched would
    /// inherit the elevation — and under an over-the-shoulder prompt it
    /// would even be the *administrator's* app, with the wrong account's
    /// config. The elevated chain reports instead, and the medium-integrity
    /// watcher relaunches.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Completion {
        /// Today's path: this process relaunches the app itself.
        RelaunchHere,
        /// Elevated path: release the guard, write the outcome, leave the
        /// relaunch to the watcher.
        ReportToWatcher,
    }

    fn install(plan: InstallPlan) -> Result<(), String> {
        install_inner(&plan, Completion::RelaunchHere)
    }

    fn install_elevated(plan: InstallPlan) -> Result<(), String> {
        install_inner(&plan, Completion::ReportToWatcher)
    }

    fn install_inner(plan: &InstallPlan, completion: Completion) -> Result<(), String> {
        log_line(&plan.log, "waiting for the tty7 GUI to exit");
        if let Err(error) = wait_for_exit(plan.parent_pid) {
            return recover_from_failed_update(plan, error, completion);
        }
        log_line(&plan.log, "re-verifying the staged Windows installer");
        let verification = match &plan.expected_sha256 {
            Some(digest) => verify_update_digest(
                &plan.installer,
                &plan.asset_name,
                &plan.expected_version,
                digest,
            ),
            None => verify_update(
                &plan.installer,
                &plan.checksums,
                &plan.asset_name,
                &plan.expected_version,
            ),
        };
        if let Err(error) = verification {
            return recover_from_failed_update(plan, error, completion);
        }

        // Setup's own PrepareToInstall repeats this, but doing it here first
        // means a directory that cannot be cleared fails with a named cause in
        // this log instead of Inno's bare "DeleteFile failed; code 5" — and the
        // previous app is relaunched instead of being left half-replaced.
        log_line(
            &plan.log,
            "stopping the tty7 daemon and clearing installed-file locks",
        );
        // From here until the relaunch, a `tty7` CLI call or a manual launch
        // must not spawn a daemon that relocks the files Setup is replacing.
        // Every path out of this function releases the guard — `launch_app`
        // on the relaunch-here paths, an explicit clear on the elevated ones.
        tty7_core::daemon::update_guard::hold();
        if let Err(error) = tty7_core::daemon::spawn::stop_for_update(&plan.install_dir) {
            return recover_from_failed_update(plan, error, completion);
        }

        log_line(&plan.log, "running the tty7 Windows installer");
        let status = match run_installer(&plan.installer, &plan.log) {
            Ok(status) => status,
            Err(error) => {
                return recover_from_failed_update(plan, error, completion);
            }
        };
        if !status.success() {
            let error = format!("the Windows installer exited with {status}");
            return recover_from_failed_update(plan, error, completion);
        }

        if let Err(error) = verify_installed_payload(&plan.install_dir, &plan.expected_version) {
            return recover_from_failed_update(plan, error, completion);
        }
        if completion == Completion::ReportToWatcher {
            log_line(
                &plan.log,
                "the Windows update completed; the watcher relaunches tty7",
            );
            // The watcher hands the outcome to the next GUI launch, so it is
            // written before the guard comes off and anything can start.
            report_outcome(
                plan.result_file.as_deref(),
                &plan.log,
                &plan.expected_version,
                &Ok(()),
            );
            tty7_core::daemon::update_guard::clear();
            queue_cleanup(&plan.install_dir, &plan.stage);
            return Ok(());
        }
        log_line(&plan.log, "the Windows update completed; relaunching tty7");
        let result = launch_app(&plan.install_dir);
        if let Err(error) = &result {
            log_line(&plan.log, error);
        }
        // A failed relaunch is the outcome here, and there is no running GUI
        // to race it; a successful one launches the new build, whose absorb
        // finds this already on disk.
        report_outcome(
            plan.result_file.as_deref(),
            &plan.log,
            &plan.expected_version,
            &result,
        );
        queue_cleanup(&plan.install_dir, &plan.stage);
        result
    }

    /// Records one terminal update failure and restores the same recovery
    /// behavior for every step that can fail after the GUI starts shutting down.
    fn recover_from_failed_update(
        plan: &InstallPlan,
        error: String,
        completion: Completion,
    ) -> Result<(), String> {
        if completion == Completion::ReportToWatcher {
            // The relaunch belongs to the watcher on this path — an elevated
            // process spawning the app is the one thing the chain must never
            // do, failure recovery included. The guard still ends here: the
            // installation is no longer being replaced.
            log_line(&plan.log, &error);
            let result = Err(error);
            // Before the guard comes off: the watcher hands this to the next
            // GUI launch, and the guard is what holds that launch back.
            report_outcome(
                plan.result_file.as_deref(),
                &plan.log,
                &plan.expected_version,
                &result,
            );
            tty7_core::daemon::update_guard::clear();
            queue_cleanup(&plan.install_dir, &plan.stage);
            return result;
        }
        recover_without_replacement(
            &plan.log,
            &plan.install_dir,
            &plan.stage,
            plan.result_file.as_deref(),
            &plan.expected_version,
            error,
        )
    }

    // ---------------------------------------------------------------------
    // The elevated chain (#504): one UAC prompt, two elevated stages, and a
    // watcher that never elevates. The GUI points the prompt at the
    // *installed* updater — the one binary a medium-integrity process cannot
    // have replaced — running `elevated-stage`; that stage re-stages the
    // payload under an admin-only directory and runs `install-elevated` from
    // it; the install stage reports through the outcome file instead of
    // relaunching the app; and the `relaunch-watcher`, spawned by the GUI
    // before the prompt as the original user, relaunches it.

    /// What `capabilities` prints, one per line — the verbs the GUI requires
    /// before it points a UAC prompt at the installed updater. The GUI keeps
    /// its own copy of this list (see `updater_speaks_elevation` in
    /// `core::update`); an updater that predates the verbs never prints them.
    const ELEVATION_CAPABILITIES: [&str; 3] =
        ["elevated-stage", "install-elevated", "relaunch-watcher"];

    struct ElevatedStagePlan {
        gui_pid: u32,
        installer: PathBuf,
        asset_name: String,
        install_dir: PathBuf,
        expected_version: String,
        log: PathBuf,
        stage: PathBuf,
        expected_sha256: String,
        status_file: PathBuf,
        result_file: Option<PathBuf>,
        config_dir: Option<PathBuf>,
    }

    fn elevated_stage(plan: ElevatedStagePlan) -> Result<(), String> {
        let result = elevated_stage_inner(&plan);
        if let Err(error) = &result
            && plan
                .result_file
                .as_deref()
                .is_some_and(|path| !path.exists())
        {
            // The install stage writes the outcome itself once it runs, so a
            // failure before that — a digest mismatch, a staging error — has
            // to be reported here, or the watcher waits out its status grace
            // for a chain that never started.
            report_outcome(
                plan.result_file.as_deref(),
                &plan.log,
                &plan.expected_version,
                &Err(error.clone()),
            );
        }
        result
    }

    fn elevated_stage_inner(plan: &ElevatedStagePlan) -> Result<(), String> {
        log_line(&plan.log, "preparing the elevated update staging");
        // Every path this stage trusts is derived from its own image, never
        // taken from the caller: the caller sits below the integrity
        // boundary, and `<install-dir>` is what the helper pinning below
        // compares against. Believing the argument would put both halves of
        // that comparison in the hands of whoever wrote the command line.
        let install_dir = installed_root()?;
        if !same_directory(&install_dir, &plan.install_dir) {
            log_line(
                &plan.log,
                &format!(
                    "the caller named {} as the installation; using {}, which is where \
                     this updater actually runs from",
                    plan.install_dir.display(),
                    install_dir.display()
                ),
            );
        }
        // The digest arrived on the command line — the one value a
        // medium-integrity process cannot have forged, because the GUI read
        // it from the release server over HTTPS. Everything this chain
        // executes or installs is checked against it, before use and again
        // after every copy.
        verify_digest(
            &plan.installer,
            &plan.expected_sha256,
            "staged Windows installer",
        )?;
        // The staged helper copy runs a process tree higher than it was
        // written, so it is pinned to the installed image first — the one
        // file a medium-integrity process cannot have replaced.
        pin_helper_to_installed(&plan.stage.join("tty7-updater.exe"), &install_dir)?;

        let staging = create_protected_staging()?;
        let result = run_install_stage(plan, &install_dir, &staging);
        // Whatever happened, the admin-only staging goes with this process. A
        // removal failure strands it for the next elevated run to clear.
        let _ = fs::remove_dir_all(&staging);
        result
    }

    /// The installation this process was started from. UAC pointed the prompt
    /// at `{app}\tty7-updater.exe`, so this process's own image names the
    /// directory a medium-integrity process cannot write — the only trust
    /// root the chain has before the payload is signed.
    fn installed_root() -> Result<PathBuf, String> {
        let exe = std::env::current_exe()
            .map_err(|error| format!("locating the running updater: {error}"))?;
        exe.parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| format!("{} names no directory", exe.display()))
    }

    /// Only for telling the log that the caller's `<install-dir>` disagreed
    /// with the image path. Deliberately not a gate: the derived directory is
    /// used either way, and a cosmetic difference (case, a short path) must
    /// not fail an update the user is watching.
    fn same_directory(left: &Path, right: &Path) -> bool {
        left.as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(&right.as_os_str().to_string_lossy())
    }

    fn run_install_stage(
        plan: &ElevatedStagePlan,
        install_dir: &Path,
        staging: &Path,
    ) -> Result<(), String> {
        let staged_installer = staging.join(&plan.asset_name);
        let staged_checksums = staging.join("checksums.txt");
        let staged_updater = staging.join("tty7-updater.exe");
        fs::copy(&plan.installer, &staged_installer).map_err(|error| {
            format!(
                "copying {} into the protected staging: {error}",
                plan.installer.display()
            )
        })?;
        fs::copy(plan.stage.join("checksums.txt"), &staged_checksums).map_err(|error| {
            format!("copying the checksums into the protected staging: {error}")
        })?;
        fs::copy(plan.stage.join("tty7-updater.exe"), &staged_updater)
            .map_err(|error| format!("copying the updater into the protected staging: {error}"))?;
        // Re-verified at their new home: the copy, not just the source, is
        // what stage 2 executes and installs.
        verify_digest(
            &staged_installer,
            &plan.expected_sha256,
            "re-staged Windows installer",
        )?;
        pin_helper_to_installed(&staged_updater, install_dir)?;

        // Name this process to the watcher as late as possible: it waits on
        // the install stage below, so its one pid covers the whole chain.
        if let Some(parent) = plan.status_file.parent() {
            let _ = fs::create_dir_all(parent);
        }
        fs::write(&plan.status_file, std::process::id().to_string())
            .map_err(|error| format!("writing {}: {error}", plan.status_file.display()))?;

        log_line(&plan.log, "running the elevated install stage");
        let mut command = Command::new(&staged_updater);
        command
            .arg("install-elevated")
            .arg(plan.gui_pid.to_string())
            .arg(&staged_installer)
            .arg(&staged_checksums)
            .arg(&plan.asset_name)
            .arg(install_dir)
            .arg(&plan.expected_version)
            .arg(&plan.log)
            .arg(&plan.stage)
            .arg("--expected-sha256")
            .arg(&plan.expected_sha256);
        if let Some(result_file) = &plan.result_file {
            command.arg("--result-file").arg(result_file);
        }
        if let Some(config_dir) = &plan.config_dir {
            command.arg("--config-dir").arg(config_dir);
        }
        let status = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .and_then(|mut child| child.wait())
            .map_err(|error| format!("running the elevated install stage: {error}"))?;
        if !status.success() {
            return Err(format!("the elevated install stage exited with {status}"));
        }
        Ok(())
    }

    /// `%ProgramData%\tty7\update-<pid>`, created with a DACL that lets no
    /// standard user in. Between the digest check and the install the payload
    /// sits in this directory, and one any medium-integrity process could
    /// write would hand it a swap-in window across exactly that gap.
    fn create_protected_staging() -> Result<PathBuf, String> {
        let program_data = std::env::var_os("ProgramData")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
        let root = program_data.join("tty7");
        claim_protected_root(&root)?;
        let staging = root.join(format!("update-{}", std::process::id()));
        create_dir_admin_only(&staging)?;
        Ok(staging)
    }

    /// Claims `%ProgramData%\tty7` itself with the admin-only DACL, taking
    /// down whatever holds the name first.
    ///
    /// `%ProgramData%` lets a standard user create directories, and the
    /// creator owns what it creates. A root left to exist as somebody's
    /// pre-created directory would hand its owner delete-child over the
    /// "admin-only" staging inside it — enough to rename the verified
    /// staging aside and drop an identically named one of their own into the
    /// gap between the digest check and the execute, which is the exact
    /// window the protected DACL exists to close. Creating the root here
    /// makes it as unwritable as the staging: `CreateDirectoryW` applies the
    /// descriptor only when it is the one creating the directory, so
    /// succeeding *is* the proof.
    ///
    /// Nothing else lives under it — it holds staging directories and
    /// nothing more — so removing it costs at most a dead chain's leftovers.
    /// A holder that cannot be cleared (a live chain's locked image, an
    /// attacker sitting on an open handle) fails the update closed.
    fn claim_protected_root(root: &Path) -> Result<(), String> {
        // A standard user racing the removal can only lose the name back to
        // us; three tries is more than that race needs.
        let mut failure = String::new();
        for _ in 0..3 {
            // `remove_path`, not `remove_dir_all`: the name may be held by a
            // file or by a junction pointing somewhere it would be a
            // catastrophe to recurse into, and a name that is not there at
            // all is the ordinary first run.
            remove_path(root)?;
            match create_dir_admin_only(root) {
                Ok(()) => return Ok(()),
                Err(error) => failure = error,
            }
        }
        Err(failure)
    }

    fn create_dir_admin_only(dir: &Path) -> Result<(), String> {
        use windows_sys::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };
        use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
        // Owner Administrators, group SYSTEM, and a protected DACL granting
        // full control to exactly those two — not even read to Users.
        const SDDL: &str = "O:BAG:SYD:P(A;OICI;FA;;;BA)(A;OICI;FA;;;SY)";
        let mut descriptor: *mut c_void = null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide_string(SDDL).as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                null_mut(),
            )
        } == 0
        {
            return Err(format!(
                "translating the staging DACL: OS error {}",
                unsafe { GetLastError() }
            ));
        }
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        let created = unsafe { CreateDirectoryW(wide_path(dir).as_ptr(), &attributes) };
        let _ = unsafe { LocalFree(descriptor) };
        if created == 0 {
            return Err(format!("creating {}: OS error {}", dir.display(), unsafe {
                GetLastError()
            }));
        }
        Ok(())
    }

    /// The staged helper copy runs elevated, so it may only be the exact
    /// bytes of the installed one: the installation directory is the trust
    /// root a medium-integrity process cannot write, and the staged copy is
    /// pinned to it before any use.
    fn pin_helper_to_installed(staged: &Path, install_dir: &Path) -> Result<(), String> {
        let installed = install_dir.join("tty7-updater.exe");
        if file_digest(staged)? != file_digest(&installed)? {
            return Err(format!(
                "the staged updater at {} does not match the installed {} — \
                 refusing to run it elevated",
                staged.display(),
                installed.display()
            ));
        }
        Ok(())
    }

    fn file_digest(path: &Path) -> Result<String, String> {
        let bytes =
            fs::read(path).map_err(|error| format!("reading {}: {error}", path.display()))?;
        Ok(tty7_core::daemon::install::checksums::hex(
            &tty7_core::daemon::install::checksums::sha256(&bytes),
        ))
    }

    fn verify_digest(file: &Path, expected_hex: &str, label: &str) -> Result<(), String> {
        let actual = file_digest(file)?;
        if !actual.eq_ignore_ascii_case(expected_hex) {
            return Err(format!(
                "the {label} failed sha256 verification: expected {expected_hex}, got {actual}"
            ));
        }
        Ok(())
    }

    /// The elevated path's replacement for `verify_update`: same filename and
    /// version checks, but the digest comes from the command line — the value
    /// the GUI read from the release server — not from a checksums file that
    /// crossed the medium-integrity staging directory beside the installer.
    fn verify_update_digest(
        installer: &Path,
        asset_name: &str,
        expected_version: &str,
        expected_sha256: &str,
    ) -> Result<(), String> {
        if installer.file_name() != Some(OsStr::new(asset_name)) {
            return Err(format!(
                "the staged installer filename does not match the release asset {asset_name:?}"
            ));
        }
        verify_digest(installer, expected_sha256, "staged Windows installer")?;
        // Same reasoning as the manifest path: corruption or replacement
        // while the helper waited is caught here, after the GUI exited.
        verify_file_version(installer, expected_version, "staged Windows installer")
    }

    struct WatcherPlan {
        status_file: PathBuf,
        result_file: PathBuf,
        app: PathBuf,
        log: PathBuf,
        version: String,
        /// The GUI that raised the prompt, so a relaunch never lands beside a
        /// window that is still there. Optional: a watcher told nothing about
        /// it still brings the app back, which is the point of the timeouts
        /// below — it just cannot tell "still up" from "already gone".
        gui_pid: Option<u32>,
    }

    /// How often the watcher looks at the status and outcome files.
    const WATCH_POLL: Duration = Duration::from_secs(1);
    /// How long the watcher waits for the chain to first report in. Covers a
    /// UAC prompt left open on the secure desktop — not a slow install, which
    /// `WATCH_TIMEOUT` bounds once the chain has reported.
    const WATCH_STATUS_GRACE: Duration = Duration::from_secs(15 * 60);
    /// Bounds the whole install once the chain reported in. An install still
    /// running past it (an antivirus rescanning every replaced file) has
    /// stopped being an install and started being a machine with no
    /// terminal on it: the watcher gives up waiting and brings the app back.
    const WATCH_TIMEOUT: Duration = Duration::from_secs(60 * 60);
    /// Between the chain's pid dying and giving up on its outcome: the write
    /// is the last thing the chain does, so it lands within seconds.
    const WATCH_RESULT_GRACE: Duration = Duration::from_secs(10);
    /// How long a GUI that is still up when the watcher is ready to relaunch
    /// is given to finish quitting. Long enough for a shutdown already under
    /// way, short enough that a GUI which is *staying* (a declined prompt
    /// whose `kill` did not land) is recognized as staying.
    const GUI_EXIT_GRACE: Duration = Duration::from_secs(30);

    /// What the watcher ended up believing about the install, and whether
    /// that belief is already on disk. Anything it had to invent has to be
    /// written before the app comes back, or a failure the chain never got
    /// to record dies with the watcher — which is the hole #540 is about.
    struct WatchOutcome {
        outcome: UpdateOutcome,
        recorded: bool,
    }

    impl WatchOutcome {
        /// The chain's own word for it, already on disk.
        fn recorded(outcome: UpdateOutcome) -> Self {
            Self {
                outcome,
                recorded: true,
            }
        }

        /// The watcher's word for it, and nobody else's.
        fn synthesized(plan: &WatcherPlan, detail: &str) -> Self {
            Self {
                outcome: UpdateOutcome {
                    version: plan.version.clone(),
                    ok: false,
                    detail: Some(detail.to_string()),
                },
                recorded: false,
            }
        }
    }

    fn watch(plan: &WatcherPlan) -> Result<(), String> {
        // Opened first thing, while the GUI is provably still alive: it is
        // blocked inside `ShellExecuteExW` waiting on the prompt, which is
        // why the watcher is spawned before the prompt is raised. A handle
        // taken then keeps naming that same process object no matter which
        // process inherits the number later.
        let gui = GuiProcess::open(plan.gui_pid);
        let started = Instant::now();
        let mut chain_seen = false;
        let end = loop {
            if let Some(end) = read_outcome_lossy(plan) {
                break end;
            }
            match status_pid(&plan.status_file) {
                Some(pid) if pid_alive(pid) => {
                    chain_seen = true;
                    if started.elapsed() > WATCH_TIMEOUT {
                        break WatchOutcome::synthesized(
                            plan,
                            "the elevated update was still running an hour after it \
                             started and never recorded a result",
                        );
                    }
                }
                // A dead pid — whether or not it was ever seen alive (a fast
                // chain can complete between two polls) — or a status file
                // that vanished after the chain was seen: the chain is over,
                // and its outcome is the last thing it writes.
                Some(_) => {
                    break await_final_outcome(plan);
                }
                None if chain_seen => {
                    break await_final_outcome(plan);
                }
                None => {
                    if started.elapsed() > WATCH_STATUS_GRACE {
                        break WatchOutcome::synthesized(
                            plan,
                            "the elevated updater never reported in; the install did \
                             not run",
                        );
                    }
                }
            }
            thread::sleep(WATCH_POLL);
        };
        finish_watch(plan, &gui, end)
    }

    /// The chain is gone; its outcome should already be on its way to disk.
    fn await_final_outcome(plan: &WatcherPlan) -> WatchOutcome {
        let deadline = Instant::now() + WATCH_RESULT_GRACE;
        loop {
            if let Some(end) = read_outcome_lossy(plan) {
                return end;
            }
            if Instant::now() >= deadline {
                return WatchOutcome::synthesized(
                    plan,
                    "the elevated updater exited without recording a result",
                );
            }
            thread::sleep(WATCH_POLL);
        }
    }

    /// The watcher's read of the outcome file: a parse failure is an outcome
    /// — something wrote it — not a reason to keep waiting. Unreadable counts
    /// as unrecorded, so the description below replaces the garbage.
    fn read_outcome_lossy(plan: &WatcherPlan) -> Option<WatchOutcome> {
        match tty7_core::daemon::install::outcome::read_outcome(&plan.result_file) {
            Ok(outcome) => outcome.map(WatchOutcome::recorded),
            Err(error) => {
                let detail = format!(
                    "the update result at {} could not be read: {error}",
                    plan.result_file.display()
                );
                log_line(&plan.log, &detail);
                Some(WatchOutcome::synthesized(plan, &detail))
            }
        }
    }

    fn finish_watch(plan: &WatcherPlan, gui: &GuiProcess, end: WatchOutcome) -> Result<(), String> {
        let _ = fs::remove_file(&plan.status_file);
        let WatchOutcome { outcome, recorded } = end;
        // Nothing relaunches over a GUI that is still on screen. Two shapes
        // reach here with one running: a declined prompt whose `kill` did not
        // land — that GUI is staying, and owns its own window and its own
        // record — and a chain that failed fast enough to beat the quitting
        // GUI out the door, which is a GUI that will be gone in a moment and
        // must be relaunched once it is. Waiting tells them apart without
        // having to know which it was.
        if gui.alive() && !gui.wait_for_exit(GUI_EXIT_GRACE) {
            log_line(
                &plan.log,
                "the tty7 that raised the prompt is still running; leaving the \
                 relaunch to it",
            );
            return Ok(());
        }
        if !recorded {
            // The chain never got far enough to say this itself, and the
            // launch below is what reads it: an unexplained update that
            // simply asks again is exactly what the outcome file is for.
            log_line(&plan.log, outcome.detail.as_deref().unwrap_or_default());
            if let Err(error) =
                tty7_core::daemon::install::outcome::write_outcome(&plan.result_file, &outcome)
            {
                log_line(
                    &plan.log,
                    &format!(
                        "could not record the update outcome at {}: {error}",
                        plan.result_file.display()
                    ),
                );
            }
        }
        // Success or failure, the binary at the app path is the one to run:
        // the new version after a completed install, the previous one after
        // a recovery. Spawned from this never-elevated process, so the app
        // comes back as the original user whatever the chain ran as.
        let health = Command::new(&plan.app)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("launching {}: {error}", plan.app.display()))
            .and_then(|mut child| healthy_after_grace(&mut child));
        if let Err(error) = health {
            // "Installed but nothing came back" is a failure the user must
            // see, so it replaces the outcome the chain recorded.
            let detail = match &outcome.detail {
                Some(previous) => format!("{previous}; and the relaunch failed: {error}"),
                None => format!("the update installed but the relaunch failed: {error}"),
            };
            let _ = tty7_core::daemon::install::outcome::write_outcome(
                &plan.result_file,
                &UpdateOutcome {
                    version: outcome.version.clone(),
                    ok: false,
                    detail: Some(detail.clone()),
                },
            );
            log_line(&plan.log, &detail);
            return Err(detail);
        }
        if outcome.ok {
            Ok(())
        } else {
            Err(outcome.detail.unwrap_or_default())
        }
    }

    /// The GUI that raised the UAC prompt, as seen by the watcher it spawned.
    ///
    /// Only ever consulted to answer "would relaunching now put a second
    /// window beside the first", and the answer is asymmetric on purpose. A
    /// living GUI always answers to its own pid, so "alive" is never wrong in
    /// the direction that would double-launch. The one way to be wrong the
    /// other way is a pid recycled into an unrelated process, which needs the
    /// GUI to have died before this watcher even started — before the prompt
    /// was answered — and costs only the relaunch this watcher would not have
    /// performed before either.
    struct GuiProcess {
        pid: Option<u32>,
        /// Held from watcher startup. A handle outlives the pid: the kernel
        /// keeps the process object alive as long as this is open, so a
        /// recycled number cannot answer for it.
        handle: Option<OwnedHandle>,
    }

    impl GuiProcess {
        fn open(pid: Option<u32>) -> Self {
            let handle = pid.and_then(|pid| {
                let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
                (!handle.is_null()).then(|| OwnedHandle(handle))
            });
            Self { pid, handle }
        }

        fn alive(&self) -> bool {
            if let Some(handle) = &self.handle {
                return unsafe { WaitForSingleObject(handle.0, 0) } == WAIT_TIMEOUT;
            }
            // The handle could not be taken. Falling back to the number keeps
            // the conservative answer available; a caller that named no pid
            // at all has no GUI to collide with.
            self.pid.is_some_and(pid_alive)
        }

        /// Waits out a GUI that is quitting. `true` once it is gone, `false`
        /// if it is still there when the budget runs out — which is the
        /// answer "this one is staying".
        fn wait_for_exit(&self, budget: Duration) -> bool {
            let deadline = Instant::now() + budget;
            loop {
                if !self.alive() {
                    return true;
                }
                if Instant::now() >= deadline {
                    return false;
                }
                thread::sleep(WATCH_POLL);
            }
        }
    }

    /// Whether the pid names a live process, from an account that may not be
    /// allowed to open it: under an over-the-shoulder elevation the chain
    /// runs as the administrator, and `ERROR_ACCESS_DENIED` is exactly the
    /// "alive" answer that boundary gives.
    fn pid_alive(pid: u32) -> bool {
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        if handle.is_null() {
            return unsafe { GetLastError() } == ERROR_ACCESS_DENIED;
        }
        let handle = OwnedHandle(handle);
        // OpenProcess succeeds for an exited process while a handle remains;
        // the zero wait tells running from merely remembered.
        let wait = unsafe { WaitForSingleObject(handle.0, 0) };
        wait == WAIT_TIMEOUT
    }

    fn status_pid(status_file: &Path) -> Option<u32> {
        fs::read_to_string(status_file).ok()?.trim().parse().ok()
    }

    fn install_portable(plan: PortableInstallPlan) -> Result<(), String> {
        install_portable_inner(&plan)
    }

    fn install_portable_inner(plan: &PortableInstallPlan) -> Result<(), String> {
        log_line(&plan.log, "waiting for the tty7 GUI to exit");
        if let Err(error) = wait_for_exit(plan.parent_pid) {
            return recover_without_replacement(
                &plan.log,
                &plan.install_dir,
                &plan.stage,
                plan.result_file.as_deref(),
                &plan.expected_version,
                error,
            );
        }

        let payload = plan.stage.join(PORTABLE_PAYLOAD_DIR);
        if let Err(error) = remove_path(&payload) {
            return recover_without_replacement(
                &plan.log,
                &plan.install_dir,
                &plan.stage,
                plan.result_file.as_deref(),
                &plan.expected_version,
                error,
            );
        }
        log_line(
            &plan.log,
            "re-verifying the staged Windows portable archive",
        );
        if let Err(error) = verify_portable_update(
            &plan.archive,
            &plan.checksums,
            &plan.asset_name,
            &plan.expected_version,
            &payload,
        ) {
            return recover_without_replacement(
                &plan.log,
                &plan.install_dir,
                &plan.stage,
                plan.result_file.as_deref(),
                &plan.expected_version,
                error,
            );
        }

        log_line(
            &plan.log,
            "stopping the tty7 daemon before replacing portable files",
        );
        // Held for the whole replacement, exactly as in `install`; the pid it
        // records must be this process's — the payload child below exits
        // immediately, and a guard naming a dead writer holds nothing.
        tty7_core::daemon::update_guard::hold();
        if let Err(error) = stop_daemon_from_payload(&payload, &plan.install_dir) {
            return recover_without_replacement(
                &plan.log,
                &plan.install_dir,
                &plan.stage,
                plan.result_file.as_deref(),
                &plan.expected_version,
                error,
            );
        }

        log_line(&plan.log, "replacing the tty7 Windows portable files");
        let report = |result: &Result<(), String>| {
            report_outcome(
                plan.result_file.as_deref(),
                &plan.log,
                &plan.expected_version,
                result,
            );
        };
        let result = replace_portable_and_relaunch(
            &plan.install_dir,
            &payload,
            |directory| {
                verify_installed_payload(directory, &plan.expected_version)?;
                launch_app(directory)
            },
            launch_app,
            &report,
        );
        if let Err(error) = &result {
            log_line(&plan.log, error);
        }
        queue_cleanup(&plan.install_dir, &plan.stage);
        result
    }

    /// Restores GUI availability when the portable files have not been moved
    /// yet, then delegates stage removal to the installed helper copy.
    fn recover_without_replacement(
        log: &Path,
        install_dir: &Path,
        stage: &Path,
        result_file: Option<&Path>,
        version: &str,
        error: String,
    ) -> Result<(), String> {
        log_line(log, &error);
        let result = Err(error);
        // The outcome lands before the old app does: the relaunched GUI
        // merges it into the update state at startup, and a write afterward
        // races that merge (#540).
        report_outcome(result_file, log, version, &result);
        let _ = launch_app(install_dir);
        queue_cleanup(install_dir, stage);
        result
    }

    fn verify_update(
        installer: &Path,
        checksums: &Path,
        asset_name: &str,
        expected_version: &str,
    ) -> Result<(), String> {
        if installer.file_name() != Some(OsStr::new(asset_name)) {
            return Err(format!(
                "the staged installer filename does not match the release asset {asset_name:?}"
            ));
        }
        verify_archive(installer, checksums, asset_name)?;
        // The release manifest and installer are published together. Repeating
        // this digest check after the GUI exits catches corruption or local
        // replacement while the helper waits to acquire the installed files.
        verify_file_version(installer, expected_version, "staged Windows installer")
    }

    fn verify_portable_update(
        archive: &Path,
        checksums: &Path,
        asset_name: &str,
        expected_version: &str,
        payload: &Path,
    ) -> Result<(), String> {
        if archive.file_name() != Some(OsStr::new(asset_name)) {
            return Err(format!(
                "the staged portable archive filename does not match the release asset \
                 {asset_name:?}"
            ));
        }
        verify_archive(archive, checksums, asset_name)?;
        extract_portable_archive(archive, payload)?;
        verify_portable_payload(payload, expected_version)
    }

    fn extract_portable_archive(archive: &Path, payload: &Path) -> Result<(), String> {
        if payload.exists() {
            return Err(format!(
                "the portable payload directory already exists: {}",
                payload.display()
            ));
        }
        let bytes =
            fs::read(archive).map_err(|error| format!("reading {}: {error}", archive.display()))?;
        let archive = smol::block_on(async_zip::base::read::mem::ZipFileReader::new(bytes))
            .map_err(|error| format!("opening the portable ZIP: {error}"))?;
        let entries = archive.file().entries();
        if entries.len() > MAX_PORTABLE_ENTRIES {
            return Err(format!(
                "the portable ZIP has {} entries; the limit is {MAX_PORTABLE_ENTRIES}",
                entries.len()
            ));
        }
        fs::create_dir(payload)
            .map_err(|error| format!("creating {}: {error}", payload.display()))?;

        let mut seen = HashSet::new();
        let mut expanded_bytes = 0u64;
        for (index, entry) in entries.iter().enumerate() {
            let name = entry
                .filename()
                .as_str()
                .map_err(|error| format!("reading portable ZIP entry {index} name: {error}"))?;
            if name.contains('\\') {
                return Err(format!(
                    "the portable ZIP path uses a non-canonical separator: {name}"
                ));
            }
            if entry
                .unix_permissions()
                .is_some_and(|mode| mode & 0o170000 == 0o120000)
            {
                return Err(format!("the portable ZIP contains a symbolic link: {name}"));
            }
            let relative = PathBuf::from(name);
            let key = portable_path_key(&relative)?;
            if !seen.insert(key) {
                return Err(format!(
                    "the portable ZIP contains a duplicate path: {}",
                    relative.display()
                ));
            }
            validate_portable_relative_path(&relative)?;
            expanded_bytes = expanded_bytes
                .checked_add(entry.uncompressed_size())
                .ok_or_else(|| "the portable ZIP expanded-size total overflowed".to_string())?;
            if expanded_bytes > MAX_PORTABLE_EXPANDED_BYTES {
                return Err(format!(
                    "the portable ZIP expands past the {} byte limit",
                    MAX_PORTABLE_EXPANDED_BYTES
                ));
            }

            let output = payload.join(&relative);
            let is_directory = entry
                .dir()
                .map_err(|error| format!("reading portable ZIP entry {name}: {error}"))?;
            if is_directory {
                if entry.uncompressed_size() != 0 {
                    return Err(format!(
                        "the portable ZIP directory entry has file data: {name}"
                    ));
                }
                fs::create_dir_all(&output)
                    .map_err(|error| format!("creating {}: {error}", output.display()))?;
                continue;
            }
            if let Some(parent) = output.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| format!("creating {}: {error}", parent.display()))?;
            }
            let mut destination = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&output)
                .map_err(|error| format!("creating {}: {error}", output.display()))?;
            let mut entry_reader = smol::block_on(archive.reader_with_entry(index))
                .map_err(|error| format!("opening portable ZIP entry {name}: {error}"))?;
            let expected_size = entry.uncompressed_size();
            let expected_crc = entry.crc32();
            smol::block_on(async {
                let mut buffer = [0u8; 64 * 1024];
                let mut written = 0u64;
                loop {
                    let read = entry_reader
                        .read(&mut buffer)
                        .await
                        .map_err(|error| format!("extracting {}: {error}", output.display()))?;
                    if read == 0 {
                        break;
                    }
                    destination
                        .write_all(&buffer[..read])
                        .map_err(|error| format!("writing {}: {error}", output.display()))?;
                    written = written.checked_add(read as u64).ok_or_else(|| {
                        format!("the extracted size overflowed for {}", output.display())
                    })?;
                    if written > expected_size {
                        return Err(format!(
                            "the portable ZIP entry expands past its declared size: {name}"
                        ));
                    }
                }
                if written != expected_size {
                    return Err(format!(
                        "the portable ZIP entry size is {written}, expected {expected_size}: {name}"
                    ));
                }
                let actual_crc = entry_reader.compute_hash();
                if actual_crc != expected_crc {
                    return Err(format!(
                        "the portable ZIP entry failed CRC32 verification: {name}"
                    ));
                }
                Ok(())
            })?;
        }
        Ok(())
    }

    fn validate_portable_relative_path(path: &Path) -> Result<(), String> {
        let mut components = path.components();
        let Some(Component::Normal(root)) = components.next() else {
            return Err(format!(
                "the portable ZIP contains an unsafe path that is not relative: {}",
                path.display()
            ));
        };
        let root = root
            .to_str()
            .ok_or_else(|| format!("the portable ZIP path is not UTF-8: {}", path.display()))?;
        if !PORTABLE_MANAGED_ROOTS.contains(&root) {
            return Err(format!(
                "the portable ZIP contains an unknown top-level entry: {root}"
            ));
        }
        validate_windows_component(root)?;
        for component in components {
            let Component::Normal(component) = component else {
                return Err(format!(
                    "the portable ZIP contains an unsafe path with a non-normal component: {}",
                    path.display()
                ));
            };
            let component = component
                .to_str()
                .ok_or_else(|| format!("the portable ZIP path is not UTF-8: {}", path.display()))?;
            validate_windows_component(component)?;
        }
        Ok(())
    }

    fn validate_windows_component(component: &str) -> Result<(), String> {
        const INVALID: [char; 9] = ['<', '>', ':', '"', '/', '\\', '|', '?', '*'];
        if component.is_empty()
            || component.ends_with(' ')
            || component.ends_with('.')
            || component.chars().any(|character| {
                character == '\0' || character < ' ' || INVALID.contains(&character)
            })
        {
            return Err(format!(
                "the portable ZIP contains an invalid Windows path component: {component:?}"
            ));
        }
        let stem = component.split('.').next().unwrap_or(component);
        let reserved = matches!(
            stem.to_ascii_uppercase().as_str(),
            "CON"
                | "PRN"
                | "AUX"
                | "NUL"
                | "COM1"
                | "COM2"
                | "COM3"
                | "COM4"
                | "COM5"
                | "COM6"
                | "COM7"
                | "COM8"
                | "COM9"
                | "LPT1"
                | "LPT2"
                | "LPT3"
                | "LPT4"
                | "LPT5"
                | "LPT6"
                | "LPT7"
                | "LPT8"
                | "LPT9"
        );
        if reserved {
            return Err(format!(
                "the portable ZIP contains a reserved Windows path component: {component:?}"
            ));
        }
        Ok(())
    }

    fn portable_path_key(path: &Path) -> Result<String, String> {
        path.components()
            .map(|component| match component {
                Component::Normal(component) => {
                    component.to_str().map(str::to_lowercase).ok_or_else(|| {
                        format!("the portable ZIP path is not UTF-8: {}", path.display())
                    })
                }
                _ => Err(format!(
                    "the portable ZIP contains an unsafe path with a non-normal component: {}",
                    path.display()
                )),
            })
            .collect::<Result<Vec<_>, _>>()
            .map(|components| components.join("/"))
    }

    fn verify_portable_payload(payload: &Path, expected_version: &str) -> Result<(), String> {
        for required in [
            "tty7-app.exe",
            "tty7.exe",
            "tty7-updater.exe",
            PORTABLE_MARKER,
            "LICENSE.txt",
            "README.md",
        ] {
            let path = payload.join(required);
            if !path.is_file() {
                return Err(format!(
                    "the portable ZIP is missing the required file {}",
                    path.display()
                ));
            }
        }
        let completions = payload.join("completions");
        if !completions.is_dir() {
            return Err(format!(
                "the portable ZIP is missing the required directory {}",
                completions.display()
            ));
        }
        let marker = fs::read(payload.join(PORTABLE_MARKER))
            .map_err(|error| format!("reading the portable marker: {error}"))?;
        if marker != PORTABLE_MARKER_CONTENT {
            return Err("the portable ZIP has an invalid .tty7-portable marker".to_string());
        }
        verify_binary_version(
            &payload.join("tty7-app.exe"),
            expected_version,
            "staged portable tty7-app.exe",
        )?;
        verify_binary_version(
            &payload.join("tty7-updater.exe"),
            expected_version,
            "staged portable tty7-updater.exe",
        )
    }

    fn verify_installed_payload(install_dir: &Path, expected_version: &str) -> Result<(), String> {
        // Validate the files at their final destination rather than trusting the
        // installer or copy operation to preserve the already-verified payload.
        for (name, label) in [
            ("tty7-app.exe", "installed tty7-app.exe"),
            ("tty7-updater.exe", "installed tty7-updater.exe"),
        ] {
            let binary = install_dir.join(name);
            if !binary.is_file() {
                return Err(format!(
                    "the Windows update did not create {}",
                    binary.display()
                ));
            }
            verify_binary_version(&binary, expected_version, label)?;
        }
        Ok(())
    }

    fn verify_archive(archive: &Path, checksums: &Path, asset_name: &str) -> Result<(), String> {
        let bytes =
            fs::read(archive).map_err(|error| format!("reading {}: {error}", archive.display()))?;
        let manifest = fs::read_to_string(checksums)
            .map_err(|error| format!("reading {}: {error}", checksums.display()))?;
        tty7_core::daemon::install::checksums::verify(&manifest, asset_name, &bytes)
            .map_err(|error| error.to_string())
    }

    fn run_installer(installer: &Path, log: &Path) -> Result<ExitStatus, String> {
        Command::new(installer)
            .args(installer_arguments(log))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|error| format!("starting {}: {error}", installer.display()))
    }

    fn stop_daemon_from_payload(payload: &Path, install_dir: &Path) -> Result<(), String> {
        let executable = payload.join("tty7-app.exe");
        let status = Command::new(&executable)
            .arg("--stop-daemon")
            .arg("--update-install-dir")
            .arg(install_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|error| {
                format!(
                    "stopping the tty7 daemon with {}: {error}",
                    executable.display()
                )
            })?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("stopping the tty7 daemon exited with {status}"))
        }
    }

    fn replace_portable_and_relaunch(
        install_dir: &Path,
        payload: &Path,
        activate_replacement: impl Fn(&Path) -> Result<(), String>,
        relaunch_previous: impl Fn(&Path) -> Result<(), String>,
        report: &impl Fn(&Result<(), String>),
    ) -> Result<(), String> {
        // A unique backup inside the portable directory is on the same volume
        // as every managed path, so moving old files aside does not degrade to
        // a cross-volume copy. Keep it explicitly: if rollback itself fails,
        // dropping a TempDir must never delete the only remaining old binary.
        //
        // Every failure arm reports its outcome before relaunching the
        // previous app: the relaunched GUI merges the outcome at startup, and
        // a write afterward races that merge (#540).
        let backup = match tempfile::Builder::new()
            .prefix(".tty7-update-backup-")
            .tempdir_in(install_dir)
        {
            Ok(backup) => backup.keep(),
            Err(error) => {
                let cause = format!(
                    "creating a portable update backup in {}: {error}",
                    install_dir.display()
                );
                // The daemon has already stopped, but no installed files have
                // moved yet. Restore GUI availability before returning the
                // backup error so every post-shutdown failure recovers alike.
                report(&Err(cause.clone()));
                return Err(with_relaunch_failure(cause, relaunch_previous(install_dir)));
            }
        };
        // Marked incomplete before any installed file moves. Nothing here
        // removes the marker on rollback: a rollback that succeeds removes
        // the whole backup, and one that fails leaves an installation whose
        // state really is suspect.
        if let Err(error) = fs::write(backup.join(PORTABLE_BACKUP_INCOMPLETE), b"") {
            let cause = format!("marking the update backup {}: {error}", backup.display());
            let _ = remove_path(&backup);
            report(&Err(cause.clone()));
            return Err(with_relaunch_failure(cause, relaunch_previous(install_dir)));
        }

        let mut moved = Vec::new();
        for root in PORTABLE_MANAGED_ROOTS {
            let current = install_dir.join(root);
            if !current.exists() {
                continue;
            }
            let previous = backup.join(root);
            if let Err(error) = fs::rename(&current, &previous) {
                let cause = format!(
                    "moving {} into the update backup: {error}",
                    current.display()
                );
                report(&Err(cause.clone()));
                let restore = restore_moved_roots(install_dir, &backup, &moved);
                let relaunch = relaunch_previous(install_dir);
                if restore.is_ok() {
                    let _ = remove_path(&backup);
                }
                return Err(recovery_error(cause, restore, relaunch, &backup));
            }
            moved.push(root);
        }

        let copy_result = PORTABLE_MANAGED_ROOTS
            .iter()
            .map(|root| (payload.join(root), install_dir.join(root)))
            .filter(|(source, _)| source.exists())
            .try_for_each(|(source, destination)| copy_path(&source, &destination));
        if let Err(error) = copy_result {
            return rollback_portable_failure(
                install_dir,
                &backup,
                error,
                &relaunch_previous,
                report,
            );
        }

        if let Err(error) = activate_replacement(install_dir) {
            return rollback_portable_failure(
                install_dir,
                &backup,
                error,
                &relaunch_previous,
                report,
            );
        }

        // The replacement survived its launch grace period. Old managed files
        // are no longer needed; an antivirus-held backup is harmless and can be
        // removed manually rather than turning a successful update into rollback.
        // The marker leaves first: a backup that outlives this process without
        // it is finished business the next launch may discard on its own.
        let _ = fs::remove_file(backup.join(PORTABLE_BACKUP_INCOMPLETE));
        let _ = remove_path(&backup);
        report(&Ok(()));
        Ok(())
    }

    fn rollback_portable_failure(
        install_dir: &Path,
        backup: &Path,
        cause: String,
        relaunch_previous: &impl Fn(&Path) -> Result<(), String>,
        report: &impl Fn(&Result<(), String>),
    ) -> Result<(), String> {
        report(&Err(cause.clone()));
        let restore = restore_portable_backup(install_dir, backup);
        let relaunch = relaunch_previous(install_dir);
        if restore.is_ok() {
            let _ = remove_path(backup);
        }
        Err(recovery_error(cause, restore, relaunch, backup))
    }

    fn restore_moved_roots(
        install_dir: &Path,
        backup: &Path,
        moved: &[&str],
    ) -> Result<(), String> {
        let mut errors = Vec::new();
        for root in moved.iter().rev() {
            let previous = backup.join(root);
            let destination = install_dir.join(root);
            if let Err(error) = fs::rename(&previous, &destination) {
                errors.push(format!(
                    "restoring {} from the update backup: {error}",
                    destination.display()
                ));
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    fn restore_portable_backup(install_dir: &Path, backup: &Path) -> Result<(), String> {
        let mut errors = Vec::new();
        for root in PORTABLE_MANAGED_ROOTS {
            let destination = install_dir.join(root);
            if let Err(error) = remove_path(&destination) {
                errors.push(error);
                continue;
            }
            let previous = backup.join(root);
            if previous.exists()
                && let Err(error) = fs::rename(&previous, &destination)
            {
                errors.push(format!(
                    "restoring {} from the update backup: {error}",
                    destination.display()
                ));
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    fn recovery_error(
        cause: String,
        restore: Result<(), String>,
        relaunch: Result<(), String>,
        backup: &Path,
    ) -> String {
        let mut message = cause;
        if let Err(error) = restore {
            message.push_str(&format!(
                "; restoring the previous portable files failed: {error}; backup preserved at {}",
                backup.display()
            ));
        }
        with_relaunch_failure(message, relaunch)
    }

    fn with_relaunch_failure(mut message: String, relaunch: Result<(), String>) -> String {
        if let Err(error) = relaunch {
            message.push_str(&format!("; relaunching the previous tty7 failed: {error}"));
        }
        message
    }

    fn copy_path(source: &Path, destination: &Path) -> Result<(), String> {
        let metadata = fs::symlink_metadata(source)
            .map_err(|error| format!("reading {}: {error}", source.display()))?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "refusing to copy a symbolic link from the portable payload: {}",
                source.display()
            ));
        }
        if metadata.is_dir() {
            fs::create_dir(destination)
                .map_err(|error| format!("creating {}: {error}", destination.display()))?;
            for entry in fs::read_dir(source)
                .map_err(|error| format!("reading {}: {error}", source.display()))?
            {
                let entry = entry.map_err(|error| {
                    format!("reading an entry in {}: {error}", source.display())
                })?;
                copy_path(&entry.path(), &destination.join(entry.file_name()))?;
            }
            return Ok(());
        }
        if metadata.is_file() {
            fs::copy(source, destination).map_err(|error| {
                format!(
                    "copying {} to {}: {error}",
                    source.display(),
                    destination.display()
                )
            })?;
            return Ok(());
        }
        Err(format!(
            "the portable payload contains an unsupported filesystem entry: {}",
            source.display()
        ))
    }

    fn installer_arguments(log: &Path) -> Vec<OsString> {
        let mut log_argument = OsString::from("/LOG=");
        log_argument.push(log);
        vec![
            OsString::from("/SP-"),
            // /SILENT, not /VERYSILENT: the install runs unattended either
            // way, but between the app quitting for the update and the
            // watcher bringing the new build up — tens of seconds, longer
            // under an antivirus scan — a very-silent install shows nothing
            // at all, and "clicked update, the app vanished" reads as a crash
            // (#600). /SILENT still asks no questions; it only keeps Inno's
            // own progress window on screen for the gap.
            OsString::from("/SILENT"),
            OsString::from("/SUPPRESSMSGBOXES"),
            OsString::from("/NORESTART"),
            OsString::from("/CLOSEAPPLICATIONS"),
            log_argument,
        ]
    }

    fn launch_app(install_dir: &Path) -> Result<(), String> {
        // Every relaunch — success, failure recovery, rollback — is a point
        // where the installation is no longer being replaced, so the daemon
        // spawn guard ends here, before the app comes up and asks for one.
        tty7_core::daemon::update_guard::clear();
        let executable = install_dir.join("tty7-app.exe");
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

    /// How long a parent that can only be *observed*, not waited on, is
    /// given to finish quitting. See `wait_for_exit_across_accounts`: past
    /// this, a recycled pid is indistinguishable from a process that never
    /// exits, and failing closed beats installing over locked files.
    const CROSS_ACCOUNT_EXIT_WAIT: Duration = Duration::from_secs(120);

    fn wait_for_exit(pid: u32) -> Result<(), String> {
        // Opening the handle before the GUI exits makes PID reuse irrelevant:
        // the kernel handle continues to name the original process object.
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        if handle.is_null() {
            let error = unsafe { GetLastError() };
            if error == ERROR_INVALID_PARAMETER {
                return Ok(());
            }
            if error == ERROR_ACCESS_DENIED {
                return wait_for_exit_across_accounts(pid);
            }
            return Err(format!("opening parent process {pid}: OS error {error}"));
        }
        let handle = OwnedHandle(handle);
        let result = unsafe { WaitForSingleObject(handle.0, INFINITE) };
        if result == WAIT_FAILED {
            return Err(format!(
                "waiting for parent process {pid}: OS error {}",
                unsafe { GetLastError() }
            ));
        }
        Ok(())
    }

    /// The wait when the parent's process object refuses this account a
    /// handle: under an over-the-shoulder elevation the chain runs as the
    /// administrator, and the signed-in user's GUI answers its `OpenProcess`
    /// with `ERROR_ACCESS_DENIED` — the same boundary `pid_alive` documents
    /// from the watcher's side. The pid is still observable across it, so
    /// the wait degrades to polling the pid until it stops answering.
    ///
    /// Bounded where the handle wait is not: without a handle, a pid
    /// recycled after the GUI exited cannot be told from a GUI that never
    /// exits, and the GUI was already quitting when this process was
    /// spawned. Running out the budget fails the attempt closed — the
    /// recovery path reports it and the watcher brings the app back —
    /// rather than letting Setup fight a window that may still hold locks.
    fn wait_for_exit_across_accounts(pid: u32) -> Result<(), String> {
        let deadline = Instant::now() + CROSS_ACCOUNT_EXIT_WAIT;
        while pid_alive(pid) {
            if Instant::now() >= deadline {
                return Err(format!(
                    "parent process {pid} was still running {} seconds after the \
                     install began",
                    CROSS_ACCOUNT_EXIT_WAIT.as_secs()
                ));
            }
            thread::sleep(WATCH_POLL);
        }
        Ok(())
    }

    struct OwnedHandle(HANDLE);

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    fn verify_file_version(path: &Path, expected: &str, label: &str) -> Result<(), String> {
        let expected = parse_version(expected)
            .ok_or_else(|| format!("the expected update version {expected:?} is invalid"))?;
        let actual = file_version(path)?;
        if actual != expected {
            return Err(format!(
                "the {label} reports version {}.{}.{} but the release expects {}.{}.{}",
                actual.0, actual.1, actual.2, expected.0, expected.1, expected.2
            ));
        }
        Ok(())
    }

    fn verify_binary_version(path: &Path, expected: &str, label: &str) -> Result<(), String> {
        verify_file_version(path, expected, label)?;
        let expected = expected.trim().trim_start_matches('v');
        let actual = product_version(path)?;
        if actual != expected {
            return Err(format!(
                "the {label} reports product version {actual:?} but the release expects {expected:?}"
            ));
        }
        Ok(())
    }

    fn file_version(path: &Path) -> Result<(u16, u16, u16), String> {
        let data = version_resource(path)?;
        let root = wide_string("\\");
        let mut value: *mut c_void = null_mut();
        let mut value_len = 0u32;
        if unsafe {
            VerQueryValueW(
                data.as_ptr() as *const c_void,
                root.as_ptr(),
                &mut value,
                &mut value_len,
            )
        } == 0
            || value.is_null()
            || value_len < size_of::<VS_FIXEDFILEINFO>() as u32
        {
            return Err(format!(
                "the version resource in {} has no fixed file information",
                path.display()
            ));
        }
        let info = unsafe { &*(value as *const VS_FIXEDFILEINFO) };
        Ok((
            (info.dwFileVersionMS >> 16) as u16,
            info.dwFileVersionMS as u16,
            (info.dwFileVersionLS >> 16) as u16,
        ))
    }

    fn product_version(path: &Path) -> Result<String, String> {
        let data = version_resource(path)?;
        let translation_path = wide_string("\\VarFileInfo\\Translation");
        let mut translations: *mut c_void = null_mut();
        let mut translations_len = 0u32;
        if unsafe {
            VerQueryValueW(
                data.as_ptr() as *const c_void,
                translation_path.as_ptr(),
                &mut translations,
                &mut translations_len,
            )
        } == 0
            || translations.is_null()
            || translations_len < 4
        {
            return Err(format!(
                "the version resource in {} has no language translation",
                path.display()
            ));
        }

        // Translation entries are two little-endian u16 values: language and
        // code page. Try every advertised string table instead of assuming the
        // common en-US/Unicode pair.
        for offset in (0..translations_len as usize).step_by(4) {
            if offset + 4 > translations_len as usize {
                break;
            }
            let entry = unsafe { (translations as *const u8).add(offset) };
            let language = u16::from_le_bytes(unsafe { [*entry, *entry.add(1)] });
            let code_page = u16::from_le_bytes(unsafe { [*entry.add(2), *entry.add(3)] });
            let query = wide_string(&format!(
                "\\StringFileInfo\\{language:04x}{code_page:04x}\\ProductVersion"
            ));
            let mut value: *mut c_void = null_mut();
            let mut value_len = 0u32;
            if unsafe {
                VerQueryValueW(
                    data.as_ptr() as *const c_void,
                    query.as_ptr(),
                    &mut value,
                    &mut value_len,
                )
            } == 0
                || value.is_null()
                || value_len == 0
            {
                continue;
            }
            let value =
                unsafe { std::slice::from_raw_parts(value as *const u16, value_len as usize) };
            let value = value.strip_suffix(&[0]).unwrap_or(value);
            return String::from_utf16(value).map_err(|error| {
                format!(
                    "the ProductVersion string in {} is invalid UTF-16: {error}",
                    path.display()
                )
            });
        }

        Err(format!(
            "the version resource in {} has no ProductVersion string",
            path.display()
        ))
    }

    fn version_resource(path: &Path) -> Result<Vec<u8>, String> {
        let wide = wide_path(path);
        let mut ignored = 0u32;
        let size = unsafe { GetFileVersionInfoSizeW(wide.as_ptr(), &mut ignored) };
        if size == 0 {
            return Err(format!(
                "reading the version resource from {}: OS error {}",
                path.display(),
                unsafe { GetLastError() }
            ));
        }
        let mut data = vec![0u8; size as usize];
        if unsafe { GetFileVersionInfoW(wide.as_ptr(), 0, size, data.as_mut_ptr() as *mut c_void) }
            == 0
        {
            return Err(format!(
                "reading the version resource from {}",
                path.display()
            ));
        }
        Ok(data)
    }

    fn parse_version(version: &str) -> Option<(u16, u16, u16)> {
        let core = version
            .trim()
            .trim_start_matches('v')
            .split(['-', '+'])
            .next()?;
        let mut parts = core.split('.');
        let result = (
            parts.next()?.parse().ok()?,
            parts.next()?.parse().ok()?,
            parts.next()?.parse().ok()?,
        );
        parts.next().is_none().then_some(result)
    }

    fn wide_path(path: &Path) -> Vec<u16> {
        path.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    fn wide_string(value: &str) -> Vec<u16> {
        OsStr::new(value)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    fn queue_cleanup(install_dir: &Path, stage: &Path) {
        // The helper cannot remove its own running image. A short-lived copy
        // from the installation waits for this process, then removes the whole
        // private stage. This needs no administrator-only delayed-delete state.
        let cleaner = install_dir.join("tty7-updater.exe");
        if Command::new(&cleaner)
            .arg("cleanup")
            .arg(std::process::id().to_string())
            .arg(stage)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .is_err()
        {
            // Preserve only the running helper when the installed cleanup copy
            // is unavailable. The small residual directory is safer than using
            // a shell command whose quoting could target the wrong path.
            let current = std::env::current_exe().ok();
            if let Ok(entries) = fs::read_dir(stage) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if current.as_deref() == Some(path.as_path()) {
                        continue;
                    }
                    let _ = if path.is_dir() {
                        fs::remove_dir_all(path)
                    } else {
                        fs::remove_file(path)
                    };
                }
            }
        }
    }

    fn remove_path(path: &Path) -> Result<(), String> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(format!("reading {}: {error}", path.display())),
        };
        let result = if metadata.is_dir() && !metadata.file_type().is_symlink() {
            fs::remove_dir_all(path)
        } else {
            fs::remove_file(path)
        };
        result.map_err(|error| format!("removing {}: {error}", path.display()))
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
        use std::cell::Cell;
        use std::os::windows::ffi::OsStringExt as _;

        #[test]
        fn parses_release_versions_for_windows_resources() {
            assert_eq!(parse_version("27.1.2"), Some((27, 1, 2)));
            assert_eq!(parse_version("v27.1.2+build.4"), Some((27, 1, 2)));
            assert_eq!(parse_version("27.1.3-nightly.20260803"), Some((27, 1, 3)));
            assert_eq!(parse_version("27.1"), None);
            assert_eq!(parse_version("27.1.2.3"), None);
        }

        #[test]
        fn reads_the_complete_product_version_from_the_current_binary() {
            let executable = std::env::current_exe().unwrap();
            assert_eq!(
                product_version(&executable).unwrap(),
                env!("CARGO_PKG_VERSION")
            );
        }

        #[test]
        fn installed_payload_verification_requires_matching_app_and_updater() {
            let root = tempfile::tempdir().unwrap();
            let executable = std::env::current_exe().unwrap();
            fs::copy(&executable, root.path().join("tty7-app.exe")).unwrap();

            let error =
                verify_installed_payload(root.path(), env!("CARGO_PKG_VERSION")).unwrap_err();
            assert!(error.contains("tty7-updater.exe"), "{error}");

            fs::copy(&executable, root.path().join("tty7-updater.exe")).unwrap();
            verify_installed_payload(root.path(), env!("CARGO_PKG_VERSION")).unwrap();
        }

        #[test]
        fn silent_installer_arguments_keep_the_log_path_native() {
            let log = Path::new(r"C:\Users\测试 User\tty7 update.log");
            let arguments = installer_arguments(log);
            assert!(
                arguments.contains(&OsString::from("/SILENT")),
                "the install is unattended, but Inno's progress window stays \
                 on screen for the gap between quit and relaunch (#600)"
            );
            assert!(
                !arguments.contains(&OsString::from("/VERYSILENT")),
                "a very-silent install leaves the screen empty for tens of seconds"
            );
            let expected: OsString = OsString::from_wide(
                &OsStr::new(r"/LOG=C:\Users\测试 User\tty7 update.log")
                    .encode_wide()
                    .collect::<Vec<_>>(),
            );
            assert!(arguments.contains(&expected));
        }

        #[test]
        fn tail_options_parse_named_arguments_in_any_order() {
            let options = tail_options(
                [
                    OsString::from("--result-file"),
                    OsString::from(r"C:\config\update-outcome.json"),
                    OsString::from("--config-dir"),
                    OsString::from(r"C:\config"),
                ]
                .into_iter(),
            )
            .unwrap();
            assert_eq!(options.config_dir.as_deref(), Some(Path::new(r"C:\config")));
            assert_eq!(
                options.result_file.as_deref(),
                Some(Path::new(r"C:\config\update-outcome.json"))
            );

            let none = tail_options(Vec::new().into_iter()).unwrap();
            assert!(none.config_dir.is_none() && none.result_file.is_none());

            // Anything unrecognized — including the positionals of an old or
            // new caller whose plans do not match this build — stays an error.
            assert!(tail_options([OsString::from("--surprise")].into_iter()).is_err());
            assert!(tail_options([OsString::from("stray")].into_iter()).is_err());
            // A flag missing its value is an error, not an empty path.
            assert!(tail_options([OsString::from("--config-dir")].into_iter()).is_err());
        }

        /// The watcher relaunches the app on every way out, so the one thing
        /// it must never get wrong is "is that GUI still on screen" — the
        /// answer that keeps a declined prompt from being handed a second
        /// window.
        #[test]
        fn a_live_gui_is_never_mistaken_for_a_finished_one() {
            // Nothing named: nothing to collide with, so a relaunch goes ahead.
            let unnamed = GuiProcess::open(None);
            assert!(!unnamed.alive());
            assert!(unnamed.wait_for_exit(Duration::from_millis(0)));

            // This process is alive and staying: "still running", which is
            // what suppresses the relaunch.
            let own = GuiProcess::open(Some(std::process::id()));
            assert!(own.alive());
            assert!(!own.wait_for_exit(Duration::from_millis(0)));

            // The case the relaunch actually depends on: a handle taken while
            // the process was alive keeps naming it, and reports the exit
            // afterwards however the pid is reused.
            let mut child = Command::new("ping")
                .args(["-n", "30", "127.0.0.1"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("ping ships with Windows");
            let gui = GuiProcess::open(Some(child.id()));
            assert!(gui.alive());
            child.kill().unwrap();
            let _ = child.wait();
            assert!(gui.wait_for_exit(Duration::from_secs(5)));
        }

        /// The elevated stage pins the helper it is about to run against the
        /// installed one, so the directory that comparison reads must come
        /// from this process's own image. Taken from `<install-dir>` instead,
        /// both sides of the comparison would belong to whoever wrote the
        /// command line — and the caller sits below the integrity boundary.
        #[test]
        fn the_installed_root_is_the_running_image_not_an_argument() {
            let exe = std::env::current_exe().unwrap();
            assert_eq!(installed_root().unwrap(), exe.parent().unwrap());

            // Only the log ever asks this, so case is all it has to forgive.
            assert!(same_directory(
                Path::new(r"C:\Program Files\tty7"),
                Path::new(r"c:\program files\TTY7")
            ));
            assert!(!same_directory(
                Path::new(r"C:\Program Files\tty7"),
                Path::new(r"C:\Users\mallory\tty7")
            ));
        }

        #[test]
        fn report_outcome_records_the_terminal_result_for_the_next_gui() {
            let root = tempfile::tempdir().unwrap();
            let outcome_path = root.path().join("update-outcome.json");
            let log = root.path().join("update.log");

            report_outcome(None, &log, "27.0.0", &Ok(()));
            assert!(
                !outcome_path.exists(),
                "a caller without --result-file gets today's behavior"
            );

            report_outcome(Some(&outcome_path), &log, "27.0.0", &Ok(()));
            let outcome = tty7_core::daemon::install::outcome::read_outcome(&outcome_path).unwrap();
            assert_eq!(
                outcome,
                Some(tty7_core::daemon::install::outcome::UpdateOutcome {
                    version: "27.0.0".to_string(),
                    ok: true,
                    detail: None,
                })
            );

            let failure: Result<(), String> = Err("the installer exited with code 5".to_string());
            report_outcome(Some(&outcome_path), &log, "27.0.0", &failure);
            let outcome = tty7_core::daemon::install::outcome::read_outcome(&outcome_path).unwrap();
            assert_eq!(
                outcome,
                Some(tty7_core::daemon::install::outcome::UpdateOutcome {
                    version: "27.0.0".to_string(),
                    ok: false,
                    detail: Some("the installer exited with code 5".to_string()),
                })
            );
        }

        #[test]
        fn archive_verification_rejects_tampered_installer_bytes() {
            let root = tempfile::tempdir().unwrap();
            let installer = root.path().join("tty7-1.0.0-windows-x86_64-setup.exe");
            let manifest = root.path().join("checksums.txt");
            fs::write(&installer, b"tampered bytes").unwrap();
            fs::write(
                &manifest,
                format!(
                    "{}  {}\n",
                    tty7_core::daemon::install::checksums::hex(
                        &tty7_core::daemon::install::checksums::sha256(b"published bytes")
                    ),
                    installer.file_name().unwrap().to_string_lossy()
                ),
            )
            .unwrap();

            let error = verify_archive(
                &installer,
                &manifest,
                installer.file_name().unwrap().to_str().unwrap(),
            )
            .unwrap_err();
            assert!(error.contains("failed sha256 verification"), "{error}");
        }

        #[test]
        fn update_verification_accepts_an_unsigned_matching_windows_binary() {
            let root = tempfile::tempdir().unwrap();
            let asset_name = format!(
                "tty7-{}-windows-x86_64-setup.exe",
                env!("CARGO_PKG_VERSION")
            );
            let installer = root.path().join(&asset_name);
            let manifest = root.path().join("checksums.txt");

            // Cargo test binaries carry the package version resource but are
            // not Authenticode-signed, making this a direct regression fixture
            // for the checksum-and-version-only update policy.
            let bytes = fs::read(std::env::current_exe().unwrap()).unwrap();
            fs::write(&installer, &bytes).unwrap();
            fs::write(
                &manifest,
                format!(
                    "{}  {asset_name}\n",
                    tty7_core::daemon::install::checksums::hex(
                        &tty7_core::daemon::install::checksums::sha256(&bytes)
                    )
                ),
            )
            .unwrap();

            verify_update(
                &installer,
                &manifest,
                &asset_name,
                env!("CARGO_PKG_VERSION"),
            )
            .unwrap();
        }

        #[test]
        fn portable_archive_verification_extracts_a_complete_release_payload() {
            let root = tempfile::tempdir().unwrap();
            let asset_name = format!("tty7-{}-windows-x86_64.zip", env!("CARGO_PKG_VERSION"));
            let archive = root.path().join(&asset_name);
            let manifest = root.path().join("checksums.txt");
            let payload = root.path().join("payload");
            let executable = fs::read(std::env::current_exe().unwrap()).unwrap();
            write_test_zip(
                &archive,
                &[
                    ("tty7-app.exe", executable.clone()),
                    ("tty7.exe", executable.clone()),
                    ("tty7-updater.exe", executable),
                    (PORTABLE_MARKER, PORTABLE_MARKER_CONTENT.to_vec()),
                    ("completions/powershell.json", b"{}".to_vec()),
                    ("LICENSE.txt", b"license".to_vec()),
                    ("README.md", b"readme".to_vec()),
                    ("conpty.dll", b"conpty".to_vec()),
                    ("OpenConsole.exe", b"openconsole".to_vec()),
                    ("LICENSE-ConPTY.txt", b"conpty license".to_vec()),
                ],
            );
            write_manifest(&archive, &manifest, &asset_name);

            verify_portable_update(
                &archive,
                &manifest,
                &asset_name,
                env!("CARGO_PKG_VERSION"),
                &payload,
            )
            .unwrap();
            assert_eq!(
                fs::read(payload.join(PORTABLE_MARKER)).unwrap(),
                PORTABLE_MARKER_CONTENT
            );
            assert!(payload.join("completions/powershell.json").is_file());
            // The bundled ConPTY has to arrive with the rest: a portable update
            // that dropped it would leave the pane host it replaced behind.
            assert!(payload.join("conpty.dll").is_file());
            assert!(payload.join("OpenConsole.exe").is_file());
        }

        #[test]
        fn portable_archive_rejects_paths_that_escape_the_payload() {
            let root = tempfile::tempdir().unwrap();
            let archive = root.path().join("unsafe.zip");
            let payload = root.path().join("payload");
            write_test_zip(&archive, &[("../outside.txt", b"escape".to_vec())]);

            let error = extract_portable_archive(&archive, &payload).unwrap_err();
            assert!(error.contains("unsafe path"), "{error}");
            assert!(!root.path().join("outside.txt").exists());
        }

        #[test]
        fn portable_archive_rejects_unknown_and_case_duplicate_paths() {
            let root = tempfile::tempdir().unwrap();
            let unknown = root.path().join("unknown.zip");
            write_test_zip(&unknown, &[("notes.txt", b"user data".to_vec())]);
            let error =
                extract_portable_archive(&unknown, &root.path().join("unknown")).unwrap_err();
            assert!(error.contains("unknown top-level entry"), "{error}");

            let duplicate = root.path().join("duplicate.zip");
            write_test_zip(
                &duplicate,
                &[
                    ("README.md", b"one".to_vec()),
                    ("readme.md", b"two".to_vec()),
                ],
            );
            let error =
                extract_portable_archive(&duplicate, &root.path().join("duplicate")).unwrap_err();
            assert!(error.contains("duplicate path"), "{error}");
        }

        #[test]
        fn portable_replacement_preserves_unmanaged_user_files() {
            let install = tempfile::tempdir().unwrap();
            let payload = tempfile::tempdir().unwrap();
            fs::write(install.path().join("tty7-app.exe"), b"old app").unwrap();
            fs::create_dir(install.path().join("completions")).unwrap();
            fs::write(
                install.path().join("completions/old.json"),
                b"old completion",
            )
            .unwrap();
            fs::write(install.path().join("my-script.ps1"), b"user file").unwrap();
            fs::write(payload.path().join("tty7-app.exe"), b"new app").unwrap();
            fs::create_dir(payload.path().join("completions")).unwrap();
            fs::write(
                payload.path().join("completions/new.json"),
                b"new completion",
            )
            .unwrap();

            replace_portable_and_relaunch(
                install.path(),
                payload.path(),
                |directory| {
                    assert_eq!(
                        fs::read(directory.join("tty7-app.exe")).unwrap(),
                        b"new app"
                    );
                    Ok(())
                },
                |_| panic!("the previous version must not relaunch after success"),
                &|_| {},
            )
            .unwrap();

            assert_eq!(
                fs::read(install.path().join("tty7-app.exe")).unwrap(),
                b"new app"
            );
            assert!(install.path().join("completions/new.json").is_file());
            assert!(!install.path().join("completions/old.json").exists());
            assert_eq!(
                fs::read(install.path().join("my-script.ps1")).unwrap(),
                b"user file"
            );
        }

        #[test]
        fn a_backup_carries_the_incomplete_marker_exactly_while_files_move() {
            let install = tempfile::tempdir().unwrap();
            let payload = tempfile::tempdir().unwrap();
            fs::write(install.path().join("tty7-app.exe"), b"old app").unwrap();
            fs::write(payload.path().join("tty7-app.exe"), b"new app").unwrap();
            let install_dir = install.path().to_path_buf();

            replace_portable_and_relaunch(
                install.path(),
                payload.path(),
                move |_| {
                    let backups: Vec<_> = fs::read_dir(&install_dir)
                        .unwrap()
                        .flatten()
                        .map(|entry| entry.path())
                        .filter(|path| {
                            path.file_name()
                                .and_then(|name| name.to_str())
                                .is_some_and(|name| name.starts_with(".tty7-update-backup-"))
                        })
                        .collect();
                    assert_eq!(backups.len(), 1, "one backup during the replacement");
                    assert!(
                        backups[0].join(PORTABLE_BACKUP_INCOMPLETE).is_file(),
                        "the marker is present while files are moving"
                    );
                    Ok(())
                },
                |_| panic!("the previous version must not relaunch after success"),
                &|_| {},
            )
            .unwrap();

            let leftovers: Vec<_> = fs::read_dir(install.path())
                .unwrap()
                .flatten()
                .map(|entry| entry.file_name())
                .collect();
            assert_eq!(
                leftovers,
                vec![std::ffi::OsString::from("tty7-app.exe")],
                "no backup and no marker survive a completed replacement"
            );
        }

        #[test]
        fn portable_replacement_rolls_back_when_the_new_app_does_not_start() {
            let install = tempfile::tempdir().unwrap();
            let payload = tempfile::tempdir().unwrap();
            fs::write(install.path().join("tty7-app.exe"), b"old app").unwrap();
            fs::write(install.path().join("tty7.exe"), b"old cli").unwrap();
            fs::write(install.path().join("notes.txt"), b"user file").unwrap();
            fs::write(payload.path().join("tty7-app.exe"), b"new app").unwrap();
            fs::write(payload.path().join("tty7.exe"), b"new cli").unwrap();
            let relaunched = Cell::new(0usize);
            let reported = Cell::new(false);

            let error = replace_portable_and_relaunch(
                install.path(),
                payload.path(),
                |_| Err("the new app exited immediately".to_string()),
                |_| {
                    // The outcome must already be on disk when the previous
                    // app comes back — the relaunched GUI reads it at startup
                    // (#540).
                    assert!(reported.get(), "the outcome is reported first");
                    relaunched.set(relaunched.get() + 1);
                    Ok(())
                },
                &|_| reported.set(true),
            )
            .unwrap_err();

            assert!(error.contains("new app exited immediately"), "{error}");
            assert_eq!(relaunched.get(), 1);
            assert_eq!(
                fs::read(install.path().join("tty7-app.exe")).unwrap(),
                b"old app"
            );
            assert_eq!(
                fs::read(install.path().join("tty7.exe")).unwrap(),
                b"old cli"
            );
            assert_eq!(
                fs::read(install.path().join("notes.txt")).unwrap(),
                b"user file"
            );
        }

        #[test]
        fn portable_replacement_relaunches_when_backup_creation_fails() {
            let root = tempfile::tempdir().unwrap();
            let install = root.path().join("not-a-directory");
            let payload = tempfile::tempdir().unwrap();
            fs::write(&install, b"unchanged installation sentinel").unwrap();
            let relaunched = Cell::new(0usize);

            let error = replace_portable_and_relaunch(
                &install,
                payload.path(),
                |_| panic!("replacement activation must not run without a backup"),
                |directory| {
                    assert_eq!(directory, install);
                    relaunched.set(relaunched.get() + 1);
                    Ok(())
                },
                &|_| {},
            )
            .unwrap_err();

            assert!(
                error.contains("creating a portable update backup"),
                "{error}"
            );
            assert_eq!(relaunched.get(), 1);
            assert_eq!(
                fs::read(&install).unwrap(),
                b"unchanged installation sentinel"
            );
        }

        fn write_test_zip(path: &Path, entries: &[(&str, Vec<u8>)]) {
            let bytes = smol::block_on(async {
                let mut output = Vec::new();
                {
                    let mut writer = async_zip::base::write::ZipFileWriter::new(&mut output);
                    for (name, bytes) in entries {
                        let options = async_zip::ZipEntryBuilder::new(
                            (*name).into(),
                            async_zip::Compression::Stored,
                        );
                        writer.write_entry_whole(options, bytes).await.unwrap();
                    }
                    writer.close().await.unwrap();
                }
                output
            });
            fs::write(path, bytes).unwrap();
        }

        fn write_manifest(archive: &Path, manifest: &Path, asset_name: &str) {
            let bytes = fs::read(archive).unwrap();
            fs::write(
                manifest,
                format!(
                    "{}  {asset_name}\n",
                    tty7_core::daemon::install::checksums::hex(
                        &tty7_core::daemon::install::checksums::sha256(&bytes)
                    )
                ),
            )
            .unwrap();
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

#[cfg(target_os = "windows")]
fn main() {
    if let Err(error) = windows::run() {
        eprintln!("tty7-updater: {error}");
        std::process::exit(1);
    }
}

#[cfg(target_os = "linux")]
fn main() {
    if let Err(error) = linux::run() {
        eprintln!("tty7-updater: {error}");
        std::process::exit(1);
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
fn main() {
    eprintln!("tty7-updater is only available on macOS, Windows and Linux");
    std::process::exit(1);
}

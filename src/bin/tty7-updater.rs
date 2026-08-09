#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]
#![cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]

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
                reject_extra(args)?;
                install(InstallPlan {
                    parent_pid,
                    current,
                    archive,
                    checksums,
                    asset_name,
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
         or: tty7-updater install <parent-pid> <current.app> <archive.zip> <checksums.txt> \
         <asset-name> <stage-dir> <version> <log-path>"
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
        archive: PathBuf,
        checksums: PathBuf,
        asset_name: String,
        stage: PathBuf,
        expected_version: String,
        log: PathBuf,
    }

    fn install(plan: InstallPlan) -> Result<(), String> {
        let replacement = plan.stage.join("unpacked/tty7.app");
        wait_for_exit(plan.parent_pid);
        log_line(&plan.log, "re-verifying staged tty7 update");
        let verification = verify_archive(&plan.archive, &plan.checksums, &plan.asset_name)
            .and_then(|()| verify_update(&plan.current, &replacement, &plan.expected_version));
        if let Err(error) = verification {
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
    use std::time::Duration;

    use smol::io::AsyncReadExt as _;

    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_INVALID_PARAMETER, GetLastError, HANDLE, WAIT_FAILED,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileVersionInfoSizeW, GetFileVersionInfoW, VS_FIXEDFILEINFO, VerQueryValueW,
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
                reject_extra(args)?;
                install(InstallPlan {
                    parent_pid,
                    installer,
                    checksums,
                    asset_name,
                    install_dir,
                    expected_version,
                    log,
                    stage,
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
                reject_extra(args)?;
                install_portable(PortableInstallPlan {
                    parent_pid,
                    archive,
                    checksums,
                    asset_name,
                    install_dir,
                    expected_version,
                    log,
                    stage,
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
            _ => Err(usage()),
        }
    }

    fn usage() -> String {
        "usage: tty7-updater verify <setup.exe> <checksums.txt> <asset-name> <version>\n\
         or: tty7-updater install <parent-pid> <setup.exe> <checksums.txt> <asset-name> \
         <install-dir> <version> <log-path> <stage-dir>\n\
         or: tty7-updater verify-portable <archive.zip> <checksums.txt> <asset-name> \
         <version> <stage-dir>\n\
         or: tty7-updater install-portable <parent-pid> <archive.zip> <checksums.txt> \
         <asset-name> <install-dir> <version> <log-path> <stage-dir>\n\
         or: tty7-updater cleanup <parent-pid> <stage-dir>"
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

    struct InstallPlan {
        parent_pid: u32,
        installer: PathBuf,
        checksums: PathBuf,
        asset_name: String,
        install_dir: PathBuf,
        expected_version: String,
        log: PathBuf,
        stage: PathBuf,
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
    }

    fn install(plan: InstallPlan) -> Result<(), String> {
        log_line(&plan.log, "waiting for the tty7 GUI to exit");
        if let Err(error) = wait_for_exit(plan.parent_pid) {
            return recover_from_failed_update(&plan, error);
        }
        log_line(&plan.log, "re-verifying the staged Windows installer");
        if let Err(error) = verify_update(
            &plan.installer,
            &plan.checksums,
            &plan.asset_name,
            &plan.expected_version,
        ) {
            return recover_from_failed_update(&plan, error);
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
        // launch_app releases the guard on every path out of this function.
        tty7_core::daemon::update_guard::hold();
        if let Err(error) = tty7_core::daemon::spawn::stop_for_update(&plan.install_dir) {
            return recover_from_failed_update(&plan, error);
        }

        log_line(&plan.log, "running the tty7 Windows installer");
        let status = match run_installer(&plan.installer, &plan.log) {
            Ok(status) => status,
            Err(error) => {
                return recover_from_failed_update(&plan, error);
            }
        };
        if !status.success() {
            let error = format!("the Windows installer exited with {status}");
            return recover_from_failed_update(&plan, error);
        }

        if let Err(error) = verify_installed_payload(&plan.install_dir, &plan.expected_version) {
            return recover_from_failed_update(&plan, error);
        }
        log_line(&plan.log, "the Windows update completed; relaunching tty7");
        let result = launch_app(&plan.install_dir);
        if let Err(error) = &result {
            log_line(&plan.log, error);
        }
        queue_cleanup(&plan.install_dir, &plan.stage);
        result
    }

    /// Records one terminal update failure and restores the same recovery
    /// behavior for every step that can fail after the GUI starts shutting down.
    fn recover_from_failed_update(plan: &InstallPlan, error: String) -> Result<(), String> {
        recover_without_replacement(&plan.log, &plan.install_dir, &plan.stage, error)
    }

    fn install_portable(plan: PortableInstallPlan) -> Result<(), String> {
        log_line(&plan.log, "waiting for the tty7 GUI to exit");
        if let Err(error) = wait_for_exit(plan.parent_pid) {
            return recover_without_replacement(&plan.log, &plan.install_dir, &plan.stage, error);
        }

        let payload = plan.stage.join(PORTABLE_PAYLOAD_DIR);
        if let Err(error) = remove_path(&payload) {
            return recover_without_replacement(&plan.log, &plan.install_dir, &plan.stage, error);
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
            return recover_without_replacement(&plan.log, &plan.install_dir, &plan.stage, error);
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
            return recover_without_replacement(&plan.log, &plan.install_dir, &plan.stage, error);
        }

        log_line(&plan.log, "replacing the tty7 Windows portable files");
        let result = replace_portable_and_relaunch(
            &plan.install_dir,
            &payload,
            |directory| {
                verify_installed_payload(directory, &plan.expected_version)?;
                launch_app(directory)
            },
            launch_app,
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
        error: String,
    ) -> Result<(), String> {
        log_line(log, &error);
        let _ = launch_app(install_dir);
        queue_cleanup(install_dir, stage);
        Err(error)
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
    ) -> Result<(), String> {
        // A unique backup inside the portable directory is on the same volume
        // as every managed path, so moving old files aside does not degrade to
        // a cross-volume copy. Keep it explicitly: if rollback itself fails,
        // dropping a TempDir must never delete the only remaining old binary.
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
            return rollback_portable_failure(install_dir, &backup, error, &relaunch_previous);
        }

        if let Err(error) = activate_replacement(install_dir) {
            return rollback_portable_failure(install_dir, &backup, error, &relaunch_previous);
        }

        // The replacement survived its launch grace period. Old managed files
        // are no longer needed; an antivirus-held backup is harmless and can be
        // removed manually rather than turning a successful update into rollback.
        // The marker leaves first: a backup that outlives this process without
        // it is finished business the next launch may discard on its own.
        let _ = fs::remove_file(backup.join(PORTABLE_BACKUP_INCOMPLETE));
        let _ = remove_path(&backup);
        Ok(())
    }

    fn rollback_portable_failure(
        install_dir: &Path,
        backup: &Path,
        cause: String,
        relaunch_previous: &impl Fn(&Path) -> Result<(), String>,
    ) -> Result<(), String> {
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
            OsString::from("/VERYSILENT"),
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

    fn wait_for_exit(pid: u32) -> Result<(), String> {
        // Opening the handle before the GUI exits makes PID reuse irrelevant:
        // the kernel handle continues to name the original process object.
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        if handle.is_null() {
            let error = unsafe { GetLastError() };
            if error == ERROR_INVALID_PARAMETER {
                return Ok(());
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
            assert!(arguments.contains(&OsString::from("/VERYSILENT")));
            let expected: OsString = OsString::from_wide(
                &OsStr::new(r"/LOG=C:\Users\测试 User\tty7 update.log")
                    .encode_wide()
                    .collect::<Vec<_>>(),
            );
            assert!(arguments.contains(&expected));
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

            let error = replace_portable_and_relaunch(
                install.path(),
                payload.path(),
                |_| Err("the new app exited immediately".to_string()),
                |_| {
                    relaunched.set(relaunched.get() + 1);
                    Ok(())
                },
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

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn main() {
    eprintln!("tty7-updater is only available on macOS and Windows");
    std::process::exit(1);
}

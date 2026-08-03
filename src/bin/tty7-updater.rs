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

#[cfg(target_os = "windows")]
mod windows {
    use std::ffi::{OsStr, OsString, c_void};
    use std::fs::{self, OpenOptions};
    use std::io::Write as _;
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt as _;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, ExitStatus, Stdio};
    use std::ptr::null_mut;
    use std::thread;
    use std::time::Duration;

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

        let installed = plan.install_dir.join("tty7-app.exe");
        if let Err(error) = verify_installed_app(&installed, &plan.expected_version) {
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
        log_line(&plan.log, &error);
        let _ = launch_app(&plan.install_dir);
        queue_cleanup(&plan.install_dir, &plan.stage);
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

    fn verify_installed_app(app: &Path, expected_version: &str) -> Result<(), String> {
        if !app.is_file() {
            return Err(format!(
                "the Windows installer did not create {}",
                app.display()
            ));
        }
        verify_file_version(app, expected_version, "installed tty7-app.exe")
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

    fn file_version(path: &Path) -> Result<(u16, u16, u16), String> {
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
        let root: Vec<u16> = OsStr::new("\\")
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
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
        use std::os::windows::ffi::OsStringExt as _;

        #[test]
        fn parses_release_versions_for_windows_resources() {
            assert_eq!(parse_version("27.1.2"), Some((27, 1, 2)));
            assert_eq!(parse_version("v27.1.2+build.4"), Some((27, 1, 2)));
            assert_eq!(parse_version("27.1"), None);
            assert_eq!(parse_version("27.1.2.3"), None);
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

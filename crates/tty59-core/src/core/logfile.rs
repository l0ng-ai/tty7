use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use log::{LevelFilter, Log, Metadata, Record};

const MAX_BYTES: u64 = 4 * 1024 * 1024;
const ARCHIVES: usize = 3;

struct FileLogger {
    role: &'static str,
    path: PathBuf,
    lock: Mutex<RepeatFilter>,
}

#[derive(Default)]
struct RepeatFilter {
    message: String,
    since: Option<std::time::Instant>,
    count: u64,
}

impl RepeatFilter {
    fn observe(&mut self, message: &str, now: std::time::Instant) -> (bool, Option<String>) {
        if self.message == message
            && self
                .since
                .is_some_and(|t| now.duration_since(t).as_secs() < 30)
        {
            self.count += 1;
            return (self.count <= 5, None);
        }
        let summary = (self.count > 5)
            .then(|| format!("{} [suppressed {} repeats]", self.message, self.count - 5));
        self.message = message.to_owned();
        self.since = Some(now);
        self.count = 1;
        (true, summary)
    }
}

impl Log for FileLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let message = format!(
            "{:5} {} pid={} [{}] {}",
            record.level(),
            self.role,
            std::process::id(),
            record.target(),
            record.args()
        );
        let Ok(mut repeats) = self.lock.lock() else {
            return;
        };
        let (emit, summary) = repeats.observe(&message, std::time::Instant::now());
        if let Some(summary) = summary {
            append(&self.path, &format!("{} {summary}\n", timestamp()));
        }
        if emit {
            append(&self.path, &format!("{} {message}\n", timestamp()));
        }
    }

    fn flush(&self) {
        if let Ok(mut repeats) = self.lock.lock() {
            if repeats.count > 5 {
                append(
                    &self.path,
                    &format!(
                        "{} {} [suppressed {} repeats]\n",
                        timestamp(),
                        repeats.message,
                        repeats.count - 5
                    ),
                );
                repeats.count = 5;
            }
        }
    }
}

pub fn install(role: &'static str) {
    let level = level_from_env();
    if level == LevelFilter::Off {
        return;
    }
    let Some(path) = log_path() else {
        return;
    };
    static LOGGER: OnceLock<FileLogger> = OnceLock::new();
    let logger = LOGGER.get_or_init(|| FileLogger {
        role,
        path,
        lock: Mutex::new(RepeatFilter::default()),
    });
    if log::set_logger(logger).is_ok() {
        log::set_max_level(level);
        log::info!(
            "process started; version={} level={level}",
            env!("CARGO_PKG_VERSION")
        );
    }
}

fn level_from_env() -> LevelFilter {
    let raw = std::env::var("TTY59_LOG")
        .or_else(|_| std::env::var("RUST_LOG"))
        .unwrap_or_default();
    parse_level(&raw)
}

fn parse_level(raw: &str) -> LevelFilter {
    match raw.trim().to_ascii_lowercase().as_str() {
        "error" => LevelFilter::Error,
        "warn" | "warning" => LevelFilter::Warn,
        "info" => LevelFilter::Info,
        "debug" => LevelFilter::Debug,
        "trace" => LevelFilter::Trace,
        "off" => LevelFilter::Off,
        _ => LevelFilter::Info,
    }
}

fn append(path: &Path, record: &str) {
    let mut end = record.len().min(MAX_BYTES as usize);
    while !record.is_char_boundary(end) {
        end -= 1;
    }
    let _ = append_rotating(path, record[..end].as_bytes(), MAX_BYTES);
}

fn private_options() -> std::fs::OpenOptions {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).read(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

fn archive(path: &Path, index: usize) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{index}"));
    PathBuf::from(name)
}

/// The lock lives in a separate file: locking the log itself would leave a
/// second process writing to a renamed inode after rotation. Never unlink it.
/// Opening the data file only under this lock also permits rotation on Windows.
pub(crate) fn append_rotating(path: &Path, bytes: &[u8], cap: u64) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut lock_name = path.as_os_str().to_owned();
    lock_name.push(".lock");
    let lock = private_options().open(PathBuf::from(lock_name))?;
    lock.lock()?;
    // A single diagnostic cannot make the log unbounded either.
    let bytes = &bytes[..bytes.len().min(cap as usize)];
    if std::fs::metadata(path).is_ok_and(|m| m.len() + bytes.len() as u64 > cap) {
        for index in (1..=ARCHIVES).rev() {
            let from = if index == 1 {
                path.to_owned()
            } else {
                archive(path, index - 1)
            };
            let to = archive(path, index);
            match std::fs::remove_file(&to) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            match std::fs::rename(from, to) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
    }
    private_options().open(path)?.write_all(bytes)
}

/// Own the stderr reader inside the daemon, so quitting the GUI cannot close
/// its pipe. Fixed-size reads bound memory even if a library never emits '\n'.
/// This captures native/library diagnostics in addition to Rust log records.
#[cfg(any(unix, windows))]
pub fn capture_stderr() -> std::io::Result<()> {
    use std::io::{Read, Write};
    static CAPTURED: OnceLock<()> = OnceLock::new();
    if CAPTURED.get().is_some() || level_from_env() == LevelFilter::Off {
        return Ok(());
    }
    let path = crate::core::config::config_path("daemon-stderr.log")
        .ok_or_else(|| std::io::Error::other("no diagnostic directory"))?;
    append_rotating(&path, b"", MAX_BYTES)?;
    // Preserve diagnostics for a foreground launcher or a caller that reads
    // stderr (including server readiness probes); detached launches inherit NUL.
    let mut original = duplicate_stderr();
    let (mut reader, writer) = std::io::pipe()?;
    // Start the reader before redirecting. A thread creation failure must not
    // leave stderr connected to a pipe with no reader.
    std::thread::Builder::new()
        .name("tty59-stderr-log".into())
        .spawn(move || {
            #[cfg(unix)]
            unsafe {
                let mut signals: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut signals);
                libc::sigaddset(&mut signals, libc::SIGTERM);
                libc::pthread_sigmask(libc::SIG_BLOCK, &signals, std::ptr::null_mut());
            }
            let mut bytes = [0; 4096];
            loop {
                match reader.read(&mut bytes) {
                    Ok(0) => break,
                    Ok(n) => {
                        if let Some(out) = original.as_mut() {
                            if out.write_all(&bytes[..n]).is_err() {
                                original = None;
                            }
                        }
                        let header = format!("{} pid={} stderr: ", timestamp(), std::process::id());
                        let mut record = header.into_bytes();
                        record.extend_from_slice(&bytes[..n]);
                        if !record.ends_with(b"\n") {
                            record.push(b'\n');
                        }
                        let _ = append_rotating(&path, &record, MAX_BYTES);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        })?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        if unsafe { libc::dup2(writer.as_raw_fd(), libc::STDERR_FILENO) } == -1 {
            return Err(std::io::Error::last_os_error());
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::{AsRawHandle, IntoRawHandle};
        use windows_sys::Win32::System::Console::{STD_ERROR_HANDLE, SetStdHandle};
        if unsafe { SetStdHandle(STD_ERROR_HANDLE, writer.as_raw_handle()) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        // The process standard handle owns this until exit.
        let _ = writer.into_raw_handle();
    }
    let _ = CAPTURED.set(());
    Ok(())
}

#[cfg(unix)]
fn duplicate_stderr() -> Option<std::fs::File> {
    use std::os::fd::FromRawFd;
    // stderr may already be closed by a detached launcher.
    let fd = unsafe { libc::fcntl(libc::STDERR_FILENO, libc::F_DUPFD_CLOEXEC, 3) };
    if fd < 0 {
        None
    } else {
        Some(unsafe { std::fs::File::from_raw_fd(fd) })
    }
}

#[cfg(windows)]
fn duplicate_stderr() -> Option<std::fs::File> {
    use std::os::windows::io::BorrowedHandle;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE};
    let handle = unsafe { GetStdHandle(STD_ERROR_HANDLE) };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return None;
    }
    unsafe { BorrowedHandle::borrow_raw(handle) }
        .try_clone_to_owned()
        .ok()
        .map(std::fs::File::from)
}

#[cfg(not(any(unix, windows)))]
pub fn capture_stderr() -> std::io::Result<()> {
    Ok(())
}

fn log_path() -> Option<PathBuf> {
    crate::core::config::config_path("tty59.log")
}

fn timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let total = now.as_secs();
    let (y, m, d) = super::crash::civil_from_days((total / 86_400) as i64);
    let secs = total % 86_400;
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60,
        now.subsec_millis()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use log::Level;

    #[test]
    fn logging_defaults_to_info_and_can_be_disabled() {
        assert_eq!(parse_level(""), LevelFilter::Info);
        assert_eq!(parse_level("   "), LevelFilter::Info);
        assert_eq!(parse_level("nonsense"), LevelFilter::Info);
        assert_eq!(parse_level("tty59_core::daemon=debug"), LevelFilter::Info);
        assert_eq!(parse_level("off"), LevelFilter::Off);
    }

    #[test]
    fn levels_parse_case_and_space_insensitively() {
        assert_eq!(parse_level("debug"), LevelFilter::Debug);
        assert_eq!(parse_level("  DEBUG "), LevelFilter::Debug);
        assert_eq!(parse_level("Warn"), LevelFilter::Warn);
        assert_eq!(parse_level("warning"), LevelFilter::Warn);
        assert_eq!(parse_level("TRACE"), LevelFilter::Trace);
    }

    #[test]
    fn rotation_keeps_the_previous_log() {
        let path = std::env::temp_dir().join(format!("tty59-logfile-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&path);

        append(&path, "first\n");
        append(&path, "second\n");
        let both = std::fs::read_to_string(&path).unwrap();
        assert!(both.contains("first") && both.contains("second"), "appends");

        std::fs::write(&path, vec![b'x'; (MAX_BYTES + 1) as usize]).unwrap();
        append(&path, "after the cap\n");
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after, "after the cap\n", "new active log");

        assert_eq!(
            std::fs::metadata(archive(&path, 1)).unwrap().len(),
            MAX_BYTES + 1
        );
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(archive(&path, 1));
    }

    #[test]
    fn rotation_retains_three_archives_and_bounds_oversized_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime.log");
        for byte in b'0'..=b'7' {
            append_rotating(&path, &[byte; 32], 16).unwrap();
        }
        assert_eq!(std::fs::read(&path).unwrap(), vec![b'7'; 16]);
        for index in 1..=3 {
            assert_eq!(
                std::fs::read(archive(&path, index)).unwrap(),
                vec![b'7' - index as u8; 16]
            );
        }
        assert!(!archive(&path, 4).exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn repeated_errors_are_summarized_without_hiding_new_errors() {
        let mut filter = RepeatFilter::default();
        let now = std::time::Instant::now();
        for n in 1..=9 {
            assert_eq!(filter.observe("connection failed", now).0, n <= 5);
        }
        let (emit, summary) = filter.observe("disk full", now);
        assert!(emit);
        assert!(summary.unwrap().contains("suppressed 4 repeats"));
        for _ in 0..8 {
            filter.observe("disk full", now);
        }
        let (emit, summary) = filter.observe("disk full", now + std::time::Duration::from_secs(30));
        assert!(emit);
        assert!(summary.is_some());
    }

    // Real processes are needed: a thread-only test could accidentally pass
    // with just FileLogger's in-process mutex protecting rotation.
    #[test]
    fn concurrent_processes_rotate_without_losing_records() {
        const VAR: &str = "TTY59_TEST_ROTATION_DIR";
        if let Some(dir) = std::env::var_os(VAR) {
            let path = PathBuf::from(dir).join("runtime.log");
            for n in 0..40 {
                append_rotating(
                    &path,
                    format!("{}:{n:03}\n", std::process::id()).as_bytes(),
                    512,
                )
                .unwrap();
            }
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mut children = Vec::new();
        for _ in 0..4 {
            children.push(
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "core::logfile::tests::concurrent_processes_rotate_without_losing_records",
                        "--nocapture",
                    ])
                    .env(VAR, dir.path())
                    .stdout(std::process::Stdio::null())
                    .spawn()
                    .unwrap(),
            );
        }
        for child in &mut children {
            assert!(child.wait().unwrap().success());
        }
        let path = dir.path().join("runtime.log");
        let mut lines = std::collections::HashSet::new();
        for p in std::iter::once(path.clone()).chain((1..=3).map(|i| archive(&path, i))) {
            if let Ok(body) = std::fs::read_to_string(p) {
                for line in body.lines() {
                    assert!(lines.insert(line.to_owned()));
                }
            }
        }
        assert_eq!(lines.len(), 160);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn stderr_capture_rotates_in_the_writing_process() {
        use std::io::Write;
        const VAR: &str = "TTY59_TEST_STDERR_DIR";
        if let Some(dir) = std::env::var_os(VAR) {
            let dir = PathBuf::from(dir);
            crate::core::config::set_config_dir(dir.clone());
            capture_stderr().unwrap();
            std::io::stderr()
                .write_all(&vec![b'x'; MAX_BYTES as usize + 4096])
                .unwrap();
            std::io::stderr()
                .write_all(b"stderr-capture-done\n")
                .unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                if std::fs::read_to_string(dir.join("daemon-stderr.log"))
                    .unwrap_or_default()
                    .contains("stderr-capture-done")
                {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "stderr was not drained"
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            println!("stdout-still-works");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "core::logfile::tests::stderr_capture_rotates_in_the_writing_process",
                "--nocapture",
            ])
            .env(VAR, dir.path())
            .env("TTY59_LOG", "info")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains("stdout-still-works"));
        assert!(String::from_utf8_lossy(&output.stderr).contains("stderr-capture-done"));
        let path = dir.path().join("daemon-stderr.log");
        assert!(archive(&path, 1).exists());
        assert!(std::fs::metadata(path).unwrap().len() <= MAX_BYTES);
    }

    #[test]
    fn a_record_names_its_role_and_target() {
        let path = std::env::temp_dir().join(format!("tty59-logrec-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let logger = FileLogger {
            role: "daemon",
            path: path.clone(),
            lock: Mutex::new(RepeatFilter::default()),
        };
        log::set_max_level(LevelFilter::Info);
        logger.log(
            &Record::builder()
                .args(format_args!("remote build-box: installed tty59-server"))
                .level(Level::Info)
                .target("tty59_core::daemon::install")
                .build(),
        );
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("daemon"), "{written}");
        assert!(written.contains("tty59_core::daemon::install"), "{written}");
        assert!(written.contains("installed tty59-server"), "{written}");
        assert!(written.contains("INFO"), "{written}");
        let _ = std::fs::remove_file(&path);
    }
}

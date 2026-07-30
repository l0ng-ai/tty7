use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use log::{LevelFilter, Log, Metadata, Record};

const MAX_BYTES: u64 = 4 * 1024 * 1024;

struct FileLogger {
    role: &'static str,
    path: PathBuf,
    lock: Mutex<()>,
}

impl Log for FileLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let mut line = String::new();
        let _ = write!(
            line,
            "{} {:5} {} [{}] {}\n",
            timestamp(),
            record.level(),
            self.role,
            record.target(),
            record.args(),
        );
        let _guard = self.lock.lock();
        append(&self.path, &line);
    }

    fn flush(&self) {}
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
        lock: Mutex::new(()),
    });
    if log::set_logger(logger).is_ok() {
        log::set_max_level(level);
    }
}

fn level_from_env() -> LevelFilter {
    let raw = std::env::var("TTY7_LOG")
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
        _ => LevelFilter::Off,
    }
}

fn append(path: &PathBuf, record: &str) {
    use std::io::Write as _;
    let truncate = std::fs::metadata(path).is_ok_and(|m| m.len() > MAX_BYTES);
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(!truncate)
        .write(true)
        .truncate(truncate)
        .open(path)
    else {
        return;
    };
    let _ = file.write_all(record.as_bytes());
    let _ = file.flush();
}

fn log_path() -> Option<PathBuf> {
    crate::core::config::config_path("tty7.log")
}

fn timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() % 86_400;
    format!(
        "{:02}:{:02}:{:02}.{:03}",
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
    fn logging_is_off_unless_asked_for() {
        assert_eq!(parse_level(""), LevelFilter::Off);
        assert_eq!(parse_level("   "), LevelFilter::Off);
        assert_eq!(parse_level("nonsense"), LevelFilter::Off);
        assert_eq!(parse_level("tty7_core::daemon=debug"), LevelFilter::Off);
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
    fn the_file_is_rewritten_once_it_passes_the_cap() {
        let path = std::env::temp_dir().join(format!("tty7-logfile-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&path);

        append(&path, "first\n");
        append(&path, "second\n");
        let both = std::fs::read_to_string(&path).unwrap();
        assert!(both.contains("first") && both.contains("second"), "appends");

        std::fs::write(&path, vec![b'x'; (MAX_BYTES + 1) as usize]).unwrap();
        append(&path, "after the cap\n");
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after, "after the cap\n", "rewritten, not appended");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_record_names_its_role_and_target() {
        let path = std::env::temp_dir().join(format!("tty7-logrec-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let logger = FileLogger {
            role: "daemon",
            path: path.clone(),
            lock: Mutex::new(()),
        };
        log::set_max_level(LevelFilter::Info);
        logger.log(
            &Record::builder()
                .args(format_args!("remote build-box: installed tty7-server"))
                .level(Level::Info)
                .target("tty7_core::daemon::install")
                .build(),
        );
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("daemon"), "{written}");
        assert!(written.contains("tty7_core::daemon::install"), "{written}");
        assert!(written.contains("installed tty7-server"), "{written}");
        assert!(written.contains("INFO"), "{written}");
        let _ = std::fs::remove_file(&path);
    }
}

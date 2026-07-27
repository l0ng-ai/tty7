//! Crash log — the panic message the OS crash reporter throws away.
//!
//! Most tty7 panics happen inside a gpui input callback, and those callbacks are
//! `extern "C"`: the panic can't unwind across them, so the runtime aborts. What
//! macOS then records is the *abort* — `panic_cannot_unwind` on top of
//! `handle_key_event` — with no message, no `file:line`, and the original frames
//! already unwound away. Reports like that are undiagnosable, and the GUI has no
//! logger and no terminal to print to.
//!
//! So we write the two lines that matter (message + location, plus a backtrace)
//! to `crash.log` in the config dir before the process goes down.

use std::fmt::Write as _;
use std::path::PathBuf;

/// Rewrite the log once it passes this, so a panic loop can't grow it forever.
const MAX_BYTES: u64 = 256 * 1024;

/// Install the panic hook for this process. `role` labels the records, since the
/// GUI and the daemon it spawns share one config dir. Chains to the previously
/// installed hook, so the usual stderr output still happens when there's a
/// terminal to see it.
pub fn install(role: &'static str) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        record(role, info);
        previous(info);
    }));
}

/// Append one record. Every step is best-effort: a panic handler that panics
/// (or fails loudly) is worse than one that loses a log line.
fn record(role: &str, info: &std::panic::PanicHookInfo<'_>) {
    let Some(path) = log_path() else {
        return;
    };
    let thread = std::thread::current();
    let mut record = String::new();
    // `info` renders as "panicked at <file:line:col>:\n<message>" — the exact
    // pair the crash report is missing.
    let _ = write!(
        record,
        "\n=== {} {} v{} pid {} thread {:?}\n{info}\n{}\n",
        utc_timestamp(),
        role,
        env!("CARGO_PKG_VERSION"),
        std::process::id(),
        thread.name().unwrap_or("<unnamed>"),
        std::backtrace::Backtrace::force_capture(),
    );
    append(&path, &record);
}

fn append(path: &PathBuf, record: &str) {
    use std::io::Write as _;
    let truncate = std::fs::metadata(path).is_ok_and(|m| m.len() > MAX_BYTES);
    let mut file = match std::fs::OpenOptions::new()
        .create(true)
        .append(!truncate)
        .write(true)
        .truncate(truncate)
        .open(path)
    {
        Ok(f) => f,
        Err(_) => return,
    };
    let _ = file.write_all(record.as_bytes());
    let _ = file.flush();
}

fn log_path() -> Option<PathBuf> {
    crate::core::config::config_path("crash.log")
}

/// `YYYY-MM-DD HH:MM:SS UTC` from the epoch seconds, so a record can be lined up
/// against an OS crash report without pulling in a date crate.
fn utc_timestamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (y, m, d) = civil_from_days(days as i64);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Howard Hinnant's `civil_from_days`: days since the Unix epoch → (y, m, d).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::{civil_from_days, install, log_path};

    /// The whole point of the hook: after a panic, the message and its location
    /// are on disk. `catch_unwind` stands in for the abort — the hook runs
    /// before either outcome.
    #[test]
    fn a_panic_lands_in_the_crash_log() {
        // Same pinned temp dir the config tests use (set-once, first call wins).
        let dir = std::env::temp_dir().join(format!("tty7-covtest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        crate::core::config::set_config_dir(dir);
        let path = log_path().expect("a pinned config dir resolves a log path");
        let _ = std::fs::remove_file(&path);

        install("test");
        let _ = std::panic::catch_unwind(|| panic!("crash-log probe"));

        let body = std::fs::read_to_string(&path).expect("the hook wrote a record");
        assert!(body.contains("crash-log probe"), "message: {body}");
        // Bare file name: `panic!`'s location carries the platform's own
        // separator (`src\core\crash.rs` on Windows).
        assert!(body.contains("crash.rs:"), "location: {body}");
        assert!(body.contains("test v"), "role + version: {body}");
    }

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_660), (2026, 7, 26));
        // Leap day, and the day after it.
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(civil_from_days(19_783), (2024, 3, 1));
    }
}

//! Stdout for a command that lives inside pipelines.
//!
//! A reader that hangs up early — `| head -1`, `| grep -q`, PowerShell's
//! `Select-Object -First` — is how a pipeline ends, not a failure. Rust makes
//! that awkward twice over: it ignores SIGPIPE at startup so the doomed write
//! comes back as an `io::Error` instead, and `println!` turns that error into a
//! panic. `tty7 capture %1 | head -1` therefore printed a panic and a backtrace
//! note where `cat` would have exited without a word — noise on stderr for a
//! correct invocation, from a CLI whose whole point is being driven by scripts
//! and agents.
//!
//! Two mechanisms, one contract:
//!
//! - On Unix [`end_pipelines_quietly`] hands the job back to the kernel. That
//!   covers every write site at once, including ones added later, and gives
//!   callers the ending they already know from `cat` (shells report 141).
//! - Windows has no SIGPIPE — the write fails with `ERROR_NO_DATA` instead — so
//!   [`out`] is the stand-in that recognizes the hang-up and leaves quietly.
//!
//! Which is why every stdout write in this binary goes through [`out`] or
//! [`line`]: the `print!` family has no way to express "the reader left".

use std::io::Write as _;

/// Exit code once the reader is gone. Unix rarely gets here — SIGPIPE has
/// already killed the process, and shells render that as 141 — so this is
/// Windows's answer, and Windows has no signal convention to imitate. Success
/// is the honest report: everything the reader asked for did reach it.
const READER_GONE: i32 = 0;

/// Let a hung-up pipe end this process the way it ends `cat`.
///
/// Called once, before anything is written, so no output can be lost to it.
#[cfg(unix)]
pub fn end_pipelines_quietly() {
    // SAFETY: setting a signal disposition is safe from single-threaded startup
    // code, and SIG_DFL is the disposition every other process starts with —
    // Rust's runtime is the thing that changed it.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

/// No-op: Windows has no SIGPIPE, so [`out`] carries the whole contract there.
#[cfg(not(unix))]
pub fn end_pipelines_quietly() {}

/// Write to stdout, leaving quietly if the reader has hung up.
///
/// Exiting from this depth is deliberate — it is what the signal does on Unix,
/// and it keeps every call site a plain statement. A `run` cut short this way
/// leaves its pane behind, same as any other interrupted `run`.
pub fn out(bytes: &[u8]) {
    let mut stdout = std::io::stdout().lock();
    // Flush here rather than at exit: `std::process::exit` runs no destructors,
    // and stdout is a LineWriter, so anything not ending in a newline would be
    // dropped on the floor.
    if let Err(e) = stdout.write_all(bytes).and_then(|()| stdout.flush()) {
        if e.kind() == std::io::ErrorKind::BrokenPipe {
            std::process::exit(READER_GONE);
        }
        // A stdout that failed for any other reason (full disk, closed fd) is
        // worth saying out loud — the caller is missing output either way, and
        // silence would make it look like there was none.
        eprintln!("tty7: writing to stdout: {e}");
        std::process::exit(1);
    }
}

/// One line and its newline, in a single write.
pub fn line(text: &str) {
    let mut buf = String::with_capacity(text.len() + 1);
    buf.push_str(text);
    buf.push('\n');
    out(buf.as_bytes());
}

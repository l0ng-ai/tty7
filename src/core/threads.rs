//! Thread-scheduling helpers shared by the daemon and the GUI client.

/// Ask the OS to schedule the calling thread at user-interactive QoS.
///
/// macOS assigns unclassified threads a default QoS the scheduler is free to
/// park on efficiency cores under load. Measured on an M1 Pro mid-benchmark:
/// whole seconds where the PTY drain drops from ~96 MB/s to 50–70 MB/s — an
/// E-core's pace — then recovers. The threads on the interactive output path
/// (daemon PTY reader, connection writer/reader, client socket reader) carry
/// keystroke echo and the visible output stream, which is exactly the workload
/// `QOS_CLASS_USER_INTERACTIVE` names. Best effort; a refused hint just keeps
/// the default class. No-op elsewhere: Linux/Windows schedulers don't demote
/// by QoS class.
pub fn promote_to_user_interactive() {
    // Escape hatch for benchmarking the promotion itself (and for users whose
    // workload fares better under default scheduling): any non-empty value
    // other than "0" disables it.
    if std::env::var("TTY7_NO_QOS").is_ok_and(|v| !v.is_empty() && v != "0") {
        return;
    }
    #[cfg(target_os = "macos")]
    // SAFETY: a plain scheduling hint for the current thread; no pointers, no
    // preconditions.
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0);
    }
}

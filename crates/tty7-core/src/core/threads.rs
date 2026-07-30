pub fn promote_to_user_interactive() {
    if std::env::var("TTY7_NO_QOS").is_ok_and(|v| !v.is_empty() && v != "0") {
        return;
    }
    #[cfg(target_os = "macos")]
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0);
    }
}

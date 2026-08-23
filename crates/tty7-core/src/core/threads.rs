pub fn promote_to_user_interactive() {
    if std::env::var("TTY7_NO_QOS").is_ok_and(|v| !v.is_empty() && v != "0") {
        return;
    }
    #[cfg(target_os = "macos")]
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0);
    }
}

/// Takes a lock, and goes on taking it after another thread has panicked
/// holding it.
///
/// `Mutex::lock().unwrap()` is the wrong default for anything the daemon owns.
/// Poisoning protects nothing: whatever inconsistency a panic left behind is
/// there either way, and the flag only decides whether the *next* thread to
/// ask also dies. In a process that holds every shell on the machine, that
/// turns one bug in one pane into a daemon that can no longer serve any of
/// them — the pty master, the child handle, the writer and the pane state all
/// sit behind mutexes, and a panic in any critical section takes the lot.
///
/// So the policy is to carry on. A garbled write to one pane is recoverable;
/// losing every session on the box is not. This is what most of the code
/// already did by hand, spelled `unwrap_or_else(|e| e.into_inner())` in about
/// seventy places, against about eighty that panicked instead — the drift is
/// what this exists to stop.
///
/// Where the inconsistency genuinely cannot be tolerated, take the poison
/// explicitly with `lock()` and decide there; the point is that it be a
/// decision rather than a default.
pub trait Locked<T> {
    /// The guard, whether or not the lock is poisoned.
    fn locked(&self) -> std::sync::MutexGuard<'_, T>;
}

impl<T> Locked<T> for std::sync::Mutex<T> {
    fn locked(&self) -> std::sync::MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::Locked as _;

    /// A mutex another thread panicked under is still usable, and still holds
    /// what that thread had written before it went.
    #[test]
    fn a_poisoned_lock_is_still_a_lock() {
        let shared = std::sync::Arc::new(std::sync::Mutex::new(vec![1, 2, 3]));
        let poisoner = std::sync::Arc::clone(&shared);
        let died = std::thread::spawn(move || {
            let mut held = poisoner.lock().expect("the first lock is clean");
            held.push(4);
            panic!("the thread goes down holding it");
        })
        .join();
        assert!(died.is_err(), "the thread did panic");
        assert!(shared.lock().is_err(), "and the mutex is poisoned");

        assert_eq!(
            *shared.locked(),
            vec![1, 2, 3, 4],
            "the lock still opens, and the write that got through is still there"
        );
        shared.locked().push(5);
        assert_eq!(*shared.locked(), vec![1, 2, 3, 4, 5], "and it stays usable");
    }

    /// Nothing the daemon owns takes a lock that can panic on poison.
    ///
    /// The policy above is only worth stating if it holds, and it drifted
    /// once already: about seventy places carried on and about eighty died,
    /// with nothing to say which was intended. A guard is cheaper than
    /// re-deciding it per review.
    ///
    /// Scoped to `daemon/` deliberately. That is the process holding every
    /// shell on the machine, where the cascade costs the most; a tool that
    /// panics takes only itself with it.
    #[test]
    fn the_daemon_takes_no_lock_that_dies_of_poison() {
        fn walk(dir: &std::path::Path, found: &mut Vec<String>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, found);
                    continue;
                }
                if path.extension().is_none_or(|e| e != "rs")
                    || path.file_name().is_some_and(|n| n == "tests.rs")
                {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let mut in_tests = false;
                for (n, line) in text.lines().enumerate() {
                    in_tests |= line.contains("#[cfg(test)]");
                    if !in_tests && line.contains(".lock().unwrap()") {
                        found.push(format!("{}:{}", path.display(), n + 1));
                    }
                }
            }
        }

        let daemon = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("daemon");
        assert!(daemon.is_dir(), "the daemon sources moved: {daemon:?}");
        let mut found = Vec::new();
        walk(&daemon, &mut found);
        assert!(
            found.is_empty(),
            "these take a lock that panics once another thread has poisoned it, \
             which is how one pane's bug becomes every pane's; use `Locked::locked` \
             or take the poison deliberately:\n{}",
            found.join("\n")
        );
    }
}

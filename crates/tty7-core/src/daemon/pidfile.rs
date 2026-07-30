use std::path::PathBuf;

use crate::core::config;

pub fn path() -> Option<PathBuf> {
    config::config_path("daemon.pid")
}

pub fn write_current() {
    let Some(path) = path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(&path, std::process::id().to_string()) {
        log::warn!("could not write pidfile {}: {e}", path.display());
    }
}

pub fn read() -> Option<u32> {
    let contents = std::fs::read_to_string(path()?).ok()?;
    contents.trim().parse::<u32>().ok()
}

pub fn remove() {
    if let Some(path) = path() {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pin_config_dir() {
        let dir = std::env::temp_dir().join(format!("tty7-covtest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        config::set_config_dir(dir);
    }

    #[test]
    fn pidfile_lifecycle_round_trips_clears_and_rejects_garbage() {
        pin_config_dir();
        write_current();
        assert_eq!(read(), Some(std::process::id()));
        remove();
        assert_eq!(read(), None, "no pid after removal");
        remove();

        std::fs::write(path().unwrap(), "not-a-pid\n").unwrap();
        assert_eq!(read(), None);
        remove();
    }
}

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context as _, Result, bail};

pub fn launch(path: Option<&Path>) -> Result<()> {
    let executable = find_executable()?;
    let mut command = Command::new(&executable);
    if let Some(path) = path {
        command.arg("--open-path").arg(path);
    }

    // The CLI must not keep a caller's redirected pipes alive after it exits.
    // The GUI owns its own logging and never needs this console's standard IO.
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("launching {}", executable.display()))?;
    Ok(())
}

fn find_executable() -> Result<PathBuf> {
    if let Some(explicit) = std::env::var_os("TTY7_APP") {
        let path = PathBuf::from(explicit);
        if path.is_file() {
            return Ok(path);
        }
        bail!(
            "TTY7_APP points to {}, but that file does not exist",
            path.display()
        );
    }

    let name = executable_name();
    if let Ok(own) = std::env::current_exe()
        && let Some(dir) = own.parent()
    {
        let sibling = dir.join(name);
        if sibling.is_file() {
            return Ok(sibling);
        }
    }

    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }

    bail!("could not find {name} next to this CLI or on PATH — install tty7-app, or set TTY7_APP")
}

fn executable_name() -> &'static str {
    if cfg!(windows) {
        "tty7-app.exe"
    } else {
        "tty7-app"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gui_executable_name_matches_the_platform() {
        assert_eq!(
            executable_name(),
            if cfg!(windows) {
                "tty7-app.exe"
            } else {
                "tty7-app"
            }
        );
    }
}

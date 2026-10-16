//! Paths belong to the target computer, not the gateway's operating system.
//! These helpers therefore use the separator advertised by that computer.

use sha2::{Digest, Sha256};
use std::io;
use tty7_core::daemon::control::WorkspaceId;

pub fn path_key(path: &str, separator: char) -> String {
    let path = if separator == '\\' {
        let path = path.replace('/', "\\").to_lowercase();
        if let Some(rest) = path.strip_prefix("\\\\?\\unc\\") {
            format!("\\\\{rest}")
        } else {
            path.trim_start_matches("\\\\?\\").to_string()
        }
    } else {
        path.to_string()
    };
    path.trim_end_matches(separator).to_string()
}

pub fn join(dir: &str, name: &str, separator: char) -> String {
    format!("{}{separator}{name}", dir.trim_end_matches(separator))
}

pub fn parent(path: &str, separator: char) -> Option<String> {
    let path = path.trim_end_matches(separator);
    if separator == '\\' {
        let plain = path.strip_prefix("\\\\?\\").unwrap_or(path);
        if plain.len() == 2 && plain.ends_with(':') {
            return None;
        }
        let unc = plain
            .strip_prefix("UNC\\")
            .or_else(|| plain.strip_prefix("unc\\"))
            .or_else(|| path.strip_prefix("\\\\"));
        if unc.is_some_and(|rest| rest.split('\\').count() <= 2) {
            return None;
        }
    }
    let at = path.rfind(separator)?;
    let end = if at == 0 {
        1
    } else if separator == '\\' && path[..at].ends_with(':') {
        at + 1
    } else {
        at
    };
    let parent = &path[..end];
    (parent != path).then(|| parent.to_string())
}

pub fn name(path: &str, separator: char) -> String {
    path.trim_end_matches(separator)
        .rsplit(separator)
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(path)
        .to_string()
}

/// Expand only the target's home shortcut. Never run a shell to interpret
/// paths, and never accept an empty/relative directory as an implicit cwd.
pub fn expand(path: &str, home: &str, separator: char) -> io::Result<String> {
    if path.is_empty() || path.contains(['\0', '\r', '\n']) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Choose a directory before starting an agent",
        ));
    }
    let path = if path == "~" {
        home.to_string()
    } else if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        join(home, rest, separator)
    } else {
        path.to_string()
    };
    let absolute = if separator == '\\' {
        path.starts_with("\\\\")
            || (path.as_bytes().get(1) == Some(&b':')
                && matches!(path.as_bytes().get(2), Some(b'\\' | b'/')))
    } else {
        path.starts_with('/')
    };
    if !absolute {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Enter an absolute directory path or a path starting with ~/",
        ));
    }
    Ok(path)
}

/// An empty project workspace remains identifiable across gateway restarts,
/// even after all its tabs were closed. Its id is private to this machine.
pub fn workspace_id(path: &str, separator: char) -> WorkspaceId {
    let mut bytes =
        Sha256::digest(format!("tty7-project-v1\0{}", path_key(path, separator)).as_bytes());
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes[..16].iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
    .parse()
    .expect("a digest is a valid workspace UUID")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_use_the_target_separator_and_home() {
        assert_eq!(
            expand("~/My Projects/tty7", "C:\\Users\\me", '\\').unwrap(),
            "C:\\Users\\me\\My Projects/tty7"
        );
        assert_eq!(
            expand("~/repo", "/home/remote", '/').unwrap(),
            "/home/remote/repo"
        );
        for path in ["", "repo", "../repo", "C:repo", "/repo\nwhoami", "/repo\0"] {
            assert!(expand(path, "/home/me", '/').is_err(), "{path:?}");
        }
        assert!(expand("C:repo", "C:\\Users\\me", '\\').is_err());
        assert_eq!(parent("/repo", '/'), Some("/".into()));
        assert_eq!(parent("/", '/'), None);
        assert_eq!(parent("C:\\repo", '\\'), Some("C:\\".into()));
        assert_eq!(parent("C:\\", '\\'), None);
        assert_eq!(parent("\\\\?\\C:\\", '\\'), None);
        assert_eq!(parent("\\\\server\\share", '\\'), None);
        assert_eq!(parent("\\\\?\\UNC\\server\\share", '\\'), None);
        assert_eq!(
            parent("\\\\server\\share\\repo", '\\'),
            Some("\\\\server\\share".into())
        );
    }

    #[test]
    fn project_identity_is_path_based_and_survives_spelling_changes() {
        assert_eq!(
            workspace_id("C:\\Projects\\tty7", '\\'),
            workspace_id("c:/projects/tty7/", '\\')
        );
        assert_eq!(
            workspace_id("/src/tty7/", '/'),
            workspace_id("/src/tty7", '/')
        );
        assert_ne!(
            workspace_id("/src/tty7", '/'),
            workspace_id("/other/tty7", '/')
        );
        assert_ne!(
            workspace_id("/src/TTY7", '/'),
            workspace_id("/src/tty7", '/')
        );
        assert_eq!(
            workspace_id("\\\\?\\UNC\\Server\\share\\repo", '\\'),
            workspace_id("\\\\server\\share\\repo", '\\')
        );
    }
}

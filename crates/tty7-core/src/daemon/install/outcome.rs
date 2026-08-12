//! The one record the auto-updater leaves behind about how an install ended.
//!
//! The updater is a separate process the GUI never waits on: the GUI quits as
//! soon as the helper is spawned, and the helper's only other channel is
//! `update.log`, which nobody reads unprompted. An install that failed
//! therefore looked exactly like one still in flight — and because launching
//! the helper had already cleared the prompt state, the same version simply
//! asked again (issue #540). This file closes that channel: the helper writes
//! it on every terminal path it can still reach, and the next GUI launch
//! merges it into the on-disk update state and deletes it.
//!
//! Both sides live in different binaries (`tty7-app` reads, `tty7-updater`
//! writes), so the schema lives here where neither can drift from the other.

use std::path::Path;

/// The filename under the config directory. The full path crosses to the
/// updater as a command-line argument — an elevated child does not inherit
/// the caller's `TTY7_CONFIG_DIR` — so this constant is only for the side
/// that reads and deletes the file.
pub const OUTCOME_FILE_NAME: &str = "update-outcome.json";

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UpdateOutcome {
    /// The version the attempt tried to install.
    pub version: String,
    pub ok: bool,
    /// Why it failed. Always present when `ok` is false — a failure without
    /// a reason is exactly the silent failure this file exists to kill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Atomic so a GUI that launches mid-write never parses half a record.
pub fn write_outcome(path: &Path, outcome: &UpdateOutcome) -> std::io::Result<()> {
    let json = serde_json::to_vec(outcome).map_err(std::io::Error::other)?;
    crate::core::config::write_atomic(path, &json)
}

/// `None` when no updater ran (the common case — the file only exists between
/// an install attempt and the next GUI launch). A file that exists but cannot
/// be read or parsed is an error: something did run, and "the result is
/// unreadable" is itself a result worth surfacing.
pub fn read_outcome(path: &Path) -> std::io::Result<Option<UpdateOutcome>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let outcome = serde_json::from_slice(&bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    Ok(Some(outcome))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_round_trips_through_the_file() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(OUTCOME_FILE_NAME);

        assert_eq!(read_outcome(&path).unwrap(), None);

        let failure = UpdateOutcome {
            version: "27.0.0".to_string(),
            ok: false,
            detail: Some("the installer exited with exit code 5".to_string()),
        };
        write_outcome(&path, &failure).unwrap();
        assert_eq!(read_outcome(&path).unwrap(), Some(failure));

        let success = UpdateOutcome {
            version: "27.0.0".to_string(),
            ok: true,
            detail: None,
        };
        write_outcome(&path, &success).unwrap();
        assert_eq!(read_outcome(&path).unwrap(), Some(success));
        // A success stays lean: no `"detail":null` for a reader to trip on.
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            r#"{"version":"27.0.0","ok":true}"#
        );
    }

    #[test]
    fn a_garbage_outcome_is_an_error_not_a_silent_miss() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(OUTCOME_FILE_NAME);
        std::fs::write(&path, b"not json").unwrap();

        assert_eq!(
            read_outcome(&path).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }
}

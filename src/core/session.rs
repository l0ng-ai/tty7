//! Session persistence: remember the tab / split-pane layout and each
//! terminal's working directory across restarts, plus a stack of recently
//! closed tabs for "Reopen Closed Tab".
//!
//! The on-disk model mirrors the live `Pane` tree but stays purely
//! serializable (no GPUI entities, no `gpui::Axis` which isn't `Serialize`).
//! It lives at `~/.config/tty7/session.json`, alongside `config.json`.
//!
//! All IO and parsing is best-effort: a missing/corrupt file just means "no
//! session to restore", and write failures are logged rather than fatal — the
//! app must never crash or stall over session bookkeeping.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Split orientation, mirroring `gpui::Axis` (which isn't `Serialize`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum SessionAxis {
    Horizontal,
    Vertical,
}

/// A serializable mirror of one tab's `Pane` tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionPane {
    /// A single terminal, restored in `cwd` (or the default dir if `None`).
    Leaf {
        #[serde(default)]
        cwd: Option<PathBuf>,
        /// Daemon pane id this leaf was mirroring. On restore we re-`attach` to
        /// it when the daemon still has it alive (process + scrollback intact),
        /// else fall back to spawning a fresh shell in `cwd`. `None` for sessions
        /// written by an older build (they just spawn fresh).
        #[serde(default)]
        pane_id: Option<u64>,
    },
    /// A split of two subtrees along `axis`, with `a` taking `ratio` of space.
    Split {
        axis: SessionAxis,
        #[serde(default = "default_ratio")]
        ratio: f32,
        a: Box<SessionPane>,
        b: Box<SessionPane>,
    },
}

fn default_ratio() -> f32 {
    0.5
}

/// A serializable mirror of one tab: its pane tree plus an optional user-set
/// name (from "Rename Tab"). A missing `name` falls back to the title-derived
/// label at render time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTab {
    #[serde(default)]
    pub name: Option<String>,
    pub pane: SessionPane,
}

/// The full saved session: the open tabs and which one was active.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    pub active: usize,
    pub tabs: Vec<SessionTab>,
}

impl Session {
    /// Load the saved session. Returns `None` when the file is absent or
    /// unreadable, and `None` (with a warning) when it fails to parse — never
    /// panics.
    pub fn load() -> Option<Session> {
        let path = Self::path()?;
        // Absent/unreadable file is the normal first-run case: silently None.
        let text = std::fs::read_to_string(&path).ok()?;
        match serde_json::from_str::<Session>(&text) {
            Ok(session) => Some(session),
            Err(e) => {
                log::warn!(
                    "failed to parse session at {}: {e}; ignoring",
                    path.display()
                );
                None
            }
        }
    }

    /// Persist the session as JSON, creating the parent directory if needed.
    /// Any IO/serialization error is logged and swallowed.
    pub fn save(&self) {
        let Some(path) = Self::path() else {
            return;
        };
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                log::warn!("failed to create session dir {}: {e}", parent.display());
                return;
            }
        }
        let json = match serde_json::to_string_pretty(self) {
            Ok(j) => j,
            Err(e) => {
                log::warn!("failed to serialize session: {e}");
                return;
            }
        };
        if let Err(e) = crate::core::config::write_atomic(&path, json.as_bytes()) {
            log::warn!("failed to write session to {}: {e}", path.display());
        }
    }

    /// `~/.config/tty7/session.json`, alongside `config.json`.
    fn path() -> Option<PathBuf> {
        crate::core::config::config_path("session.json")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin the process config dir at a shared temp location so `save`/`load`
    /// (which resolve `session.json` under it) never touch the real `~/.config`.
    /// `set_config_dir` is first-call-wins; every IO test computes the same path.
    fn pin_config_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tty7-covtest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        crate::core::config::set_config_dir(dir.clone());
        dir
    }

    #[test]
    fn session_json_round_trips_nested_tree() {
        let session = Session {
            active: 1,
            tabs: vec![
                SessionTab {
                    name: Some("build".into()),
                    pane: SessionPane::Leaf {
                        cwd: Some(PathBuf::from("/work")),
                        pane_id: Some(7),
                    },
                },
                SessionTab {
                    name: None,
                    pane: SessionPane::Split {
                        axis: SessionAxis::Vertical,
                        ratio: 0.3,
                        a: Box::new(SessionPane::Leaf {
                            cwd: None,
                            pane_id: None,
                        }),
                        b: Box::new(SessionPane::Leaf {
                            cwd: Some(PathBuf::from("/tmp")),
                            pane_id: Some(9),
                        }),
                    },
                },
            ],
        };
        let json = serde_json::to_string(&session).unwrap();
        let back: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(back.active, 1);
        assert_eq!(back.tabs.len(), 2);
        assert!(matches!(
            back.tabs[0].pane,
            SessionPane::Leaf {
                pane_id: Some(7),
                ..
            }
        ));
        match &back.tabs[1].pane {
            SessionPane::Split { ratio, .. } => assert!((ratio - 0.3).abs() < 1e-6),
            _ => panic!("expected a split"),
        }
    }

    #[test]
    fn session_defaults_fill_missing_fields() {
        // An empty object → default (active 0, no tabs).
        let s: Session = serde_json::from_str("{}").unwrap();
        assert_eq!(s.active, 0);
        assert!(s.tabs.is_empty());

        // A split without a ratio falls back to the 0.5 default, and a leaf
        // without cwd/pane_id decodes with `None`s.
        let pane: SessionPane = serde_json::from_str(
            r#"{"Split":{"axis":"Horizontal","a":{"Leaf":{}},"b":{"Leaf":{}}}}"#,
        )
        .unwrap();
        match pane {
            SessionPane::Split { ratio, .. } => assert_eq!(ratio, 0.5),
            _ => panic!("expected split"),
        }
    }

    #[test]
    fn save_then_load_recovers_the_session() {
        pin_config_dir();
        let session = Session {
            active: 0,
            tabs: vec![SessionTab {
                name: Some("main".into()),
                pane: SessionPane::Leaf {
                    cwd: Some(PathBuf::from("/home/u")),
                    pane_id: Some(1),
                },
            }],
        };
        session.save();
        let loaded = Session::load().expect("a saved session should load back");
        assert_eq!(loaded.tabs.len(), 1);
        assert_eq!(loaded.tabs[0].name.as_deref(), Some("main"));
    }
}

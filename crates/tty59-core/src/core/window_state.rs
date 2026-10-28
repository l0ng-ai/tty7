use serde::{Deserialize, Serialize};

const MIN_SIZE: f32 = 200.0;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowState {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl WindowState {
    fn path() -> Option<std::path::PathBuf> {
        crate::core::config::config_path("window.json")
    }

    pub fn load() -> Option<Self> {
        let path = Self::path()?;
        let text = std::fs::read_to_string(&path).ok()?;
        let state: Self = serde_json::from_str(&text)
            .map_err(|e| log::warn!("failed to parse {}: {e}; ignoring", path.display()))
            .ok()?;
        state.is_usable().then_some(state)
    }

    fn is_usable(&self) -> bool {
        [self.x, self.y, self.width, self.height]
            .iter()
            .all(|v| v.is_finite())
            && self.width >= MIN_SIZE
            && self.height >= MIN_SIZE
    }

    pub fn save(&self) {
        let Some(path) = Self::path() else {
            return;
        };
        let json = match serde_json::to_string_pretty(self) {
            Ok(j) => j,
            Err(e) => {
                log::warn!("failed to serialize window state: {e}");
                return;
            }
        };
        if let Err(e) = crate::core::config::write_atomic(&path, json.as_bytes()) {
            log::warn!("failed to write {}: {e}", path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_degenerate_geometry() {
        let usable =
            |json: &str| serde_json::from_str::<WindowState>(json).is_ok_and(|s| s.is_usable());
        assert!(usable(r#"{"x":-120.5,"y":42,"width":1440,"height":900}"#));
        assert!(!usable(r#"{"x":0,"y":0,"width":50,"height":900}"#));
        assert!(!usable(r#"{"x":null,"y":0,"width":1440,"height":900}"#));
        assert!(!usable("not json"));
    }
}

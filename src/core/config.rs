pub use tty7_core::core::config::*;

pub use tty7_core::core::config::Config as CoreConfig;

#[derive(Debug, Clone, Default)]
pub struct Config(pub CoreConfig);

impl gpui::Global for Config {}

impl Config {
    pub fn load() -> Self {
        #[cfg(test)]
        assert_scratch_config_dir("Config::load");
        Self(CoreConfig::load())
    }

    #[cfg(test)]
    pub fn save(&self) {
        assert_scratch_config_dir("Config::save");
        self.0.save();
    }
}

#[cfg(test)]
fn is_real_user_config_dir(dir: Option<&std::path::Path>) -> bool {
    dir.is_some() && dir == default_config_dir().as_deref()
}

#[cfg(test)]
fn assert_scratch_config_dir(what: &str) {
    assert!(
        !is_real_user_config_dir(config_dir_path().as_deref()),
        "{what} in a test would touch the real user config dir ({}). \
         Call `crate::core::config::pin_test_config_dir()` \
         at the top of the test/harness first.",
        config_dir_path().unwrap_or_default().display(),
    );
}

#[cfg(test)]
pub(crate) fn pin_test_config_dir() {
    let dir = std::env::temp_dir().join(format!("tty7-covtest-{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok();
    set_config_dir(dir);
}

impl std::ops::Deref for Config {
    type Target = CoreConfig;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for Config {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl From<CoreConfig> for Config {
    fn from(config: CoreConfig) -> Self {
        Self(config)
    }
}

pub fn gpui_font_features(features: &FontFeatures) -> gpui::FontFeatures {
    gpui::FontFeatures(std::sync::Arc::new(features.tag_value_list().to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn the_real_config_dir_is_the_only_one_the_guard_rejects() {
        if let Some(real) = default_config_dir() {
            assert!(is_real_user_config_dir(Some(&real)));
            assert!(!is_real_user_config_dir(Some(&real.join("scratch"))));
        }
        assert!(!is_real_user_config_dir(Some(Path::new(
            "/tmp/tty7-scratch"
        ))));
        assert!(!is_real_user_config_dir(None));
    }

    #[test]
    fn pinning_lands_outside_the_real_config_dir() {
        pin_test_config_dir();
        assert!(!is_real_user_config_dir(config_dir_path().as_deref()));
    }

    #[test]
    fn font_features_match_gpui_byte_for_byte() {
        const JSON: &str = r#"{"calt":true,"liga":1,"ss01":0,"zero":false,"bad":1,"kern":null}"#;

        let ours: FontFeatures = serde_json::from_str(JSON).unwrap();
        let theirs: gpui::FontFeatures = serde_json::from_str(JSON).unwrap();

        assert_eq!(ours.tag_value_list(), theirs.tag_value_list());
        assert_eq!(ours.is_calt_enabled(), theirs.is_calt_enabled());
        assert_eq!(
            serde_json::to_string(&ours).unwrap(),
            serde_json::to_string(&theirs).unwrap()
        );
        assert_eq!(gpui_font_features(&ours), theirs);
    }
}

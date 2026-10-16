use settings::{RegisterSetting, Settings, SettingsContent};
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq, RegisterSetting)]
pub struct LightpandaSettings {
    pub enabled: bool,
    pub binary: PathBuf,
    pub port: u16,
    pub obey_robots: bool,
}

pub fn default_binary() -> PathBuf {
    paths::home_dir().join(".local/bin/lightpanda")
}

impl Settings for LightpandaSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let content = content.lightpanda.clone().unwrap_or_default();
        Self {
            enabled: content.enabled.unwrap_or(true),
            binary: content
                .binary
                .filter(|binary| !binary.trim().is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(default_binary),
            port: content.port.unwrap_or(9222),
            obey_robots: content.obey_robots.unwrap_or(true),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn defaults_resolve_from_default_json(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
            let settings = LightpandaSettings::get_global(cx);
            assert!(settings.enabled, "Lightpanda is on by default");
            assert_eq!(settings.port, 9222);
            assert!(settings.obey_robots);
            assert!(settings.binary.ends_with(".local/bin/lightpanda"));
        });
    }
}

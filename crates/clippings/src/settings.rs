use settings::{RegisterSetting, Settings, SettingsContent};

#[derive(Clone, Debug, PartialEq, Eq, RegisterSetting)]
pub struct ClippingsSettings {
    /// `"saga": { "save_clippings": true }`: captures become files under
    /// `.hulder/clippings/`; when false they open in scratch buffers.
    pub save_clippings: bool,
}

impl Settings for ClippingsSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let saga = content.saga.clone().unwrap_or_default();
        Self {
            save_clippings: saga.save_clippings.unwrap_or(true),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn save_clippings_defaults_on_and_can_be_turned_off(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
            assert!(ClippingsSettings::get_global(cx).save_clippings);
        });
        cx.update_global(|store: &mut settings::SettingsStore, cx| {
            store
                .set_user_settings(r#"{ "saga": { "save_clippings": false } }"#, cx)
                .expect("valid settings");
        });
        cx.update(|cx| assert!(!ClippingsSettings::get_global(cx).save_clippings));
    }
}

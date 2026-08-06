use std::env;
use std::rc::Rc;

use dogi_core::Result;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ApplicationLanguage {
    #[default]
    System,
    English,
    SimplifiedChinese,
}

impl ApplicationLanguage {
    pub(crate) fn from_index(index: i32) -> Self {
        match index {
            1 => Self::English,
            2 => Self::SimplifiedChinese,
            _ => Self::System,
        }
    }

    pub(crate) fn index(self) -> i32 {
        match self {
            Self::System => 0,
            Self::English => 1,
            Self::SimplifiedChinese => 2,
        }
    }

    pub(crate) fn locale(self) -> &'static str {
        match self {
            Self::System => system_locale(),
            Self::English => "en",
            Self::SimplifiedChinese => "zh_CN",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ApplicationTheme {
    #[default]
    System,
    Light,
    Dark,
}

impl ApplicationTheme {
    pub(crate) fn from_index(index: i32) -> Self {
        match index {
            1 => Self::Light,
            2 => Self::Dark,
            _ => Self::System,
        }
    }

    pub(crate) fn index(self) -> i32 {
        match self {
            Self::System => 0,
            Self::Light => 1,
            Self::Dark => 2,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CloseBehavior {
    #[default]
    Quit,
    MinimizeToTray,
}

impl CloseBehavior {
    pub(crate) fn from_index(index: i32) -> Self {
        match index {
            1 => Self::MinimizeToTray,
            _ => Self::Quit,
        }
    }

    pub(crate) fn index(self) -> i32 {
        match self {
            Self::Quit => 0,
            Self::MinimizeToTray => 1,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ApplicationPreferences {
    pub language: ApplicationLanguage,
    pub theme: ApplicationTheme,
    pub close_behavior: CloseBehavior,
    pub background_operations_enabled: bool,
    pub automatic_update_checks_enabled: bool,
}

impl Default for ApplicationPreferences {
    fn default() -> Self {
        Self {
            language: ApplicationLanguage::System,
            theme: ApplicationTheme::System,
            close_behavior: CloseBehavior::Quit,
            background_operations_enabled: true,
            automatic_update_checks_enabled: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApplicationPreferenceChange {
    Language(ApplicationLanguage),
    Theme(ApplicationTheme),
    CloseBehavior(CloseBehavior),
    BackgroundOperationsEnabled(bool),
    AutomaticUpdateChecksEnabled(bool),
}

pub type ApplicationPreferenceSaver = Rc<dyn Fn(ApplicationPreferenceChange) -> Result<()>>;

#[derive(Clone)]
pub struct ApplicationPreferencesIntegration {
    pub initial: ApplicationPreferences,
    pub load_error: Option<String>,
    pub save: ApplicationPreferenceSaver,
}

impl ApplicationPreferencesIntegration {
    pub fn new(
        initial: ApplicationPreferences,
        save: impl Fn(ApplicationPreferenceChange) -> Result<()> + 'static,
    ) -> Self {
        Self {
            initial,
            load_error: None,
            save: Rc::new(save),
        }
    }

    pub fn with_load_error(mut self, error: impl Into<String>) -> Self {
        self.load_error = Some(error.into());
        self
    }
}

impl Default for ApplicationPreferencesIntegration {
    fn default() -> Self {
        Self::new(ApplicationPreferences::default(), |_| Ok(()))
    }
}

fn system_locale() -> &'static str {
    ["LANGUAGE", "LC_ALL", "LC_MESSAGES", "LANG"]
        .into_iter()
        .filter_map(|name| env::var(name).ok())
        .flat_map(|value| value.split(':').map(str::to_owned).collect::<Vec<_>>())
        .find_map(|locale| normalized_supported_locale(&locale))
        .unwrap_or("en")
}

fn normalized_supported_locale(locale: &str) -> Option<&'static str> {
    let locale = locale.split(['.', '@']).next().unwrap_or(locale);
    let normalized = locale.replace('-', "_").to_ascii_lowercase();
    match normalized.as_str() {
        "zh_cn" | "zh_sg" | "zh_hans" | "zh_hans_cn" | "zh_hans_sg" => Some("zh_CN"),
        "c" | "posix" | "en" => Some("en"),
        value if value.starts_with("en_") => Some("en"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locale_normalization_only_maps_supported_languages() {
        assert_eq!(normalized_supported_locale("zh_CN.UTF-8"), Some("zh_CN"));
        assert_eq!(normalized_supported_locale("zh-Hans-CN"), Some("zh_CN"));
        assert_eq!(normalized_supported_locale("en_US.UTF-8"), Some("en"));
        assert_eq!(normalized_supported_locale("zh_TW.UTF-8"), None);
        assert_eq!(normalized_supported_locale("de_DE.UTF-8"), None);
    }
}

use std::env;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use dogi_ui::{
    ApplicationLanguage, ApplicationPreferenceChange, ApplicationPreferences, ApplicationTheme,
    CloseBehavior,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::desktop;

const APP_CONFIG_SCHEMA_VERSION: u16 = 1;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StoredLanguage {
    #[default]
    System,
    English,
    SimplifiedChinese,
}

impl StoredLanguage {
    fn application_value(self) -> ApplicationLanguage {
        match self {
            Self::System => ApplicationLanguage::System,
            Self::English => ApplicationLanguage::English,
            Self::SimplifiedChinese => ApplicationLanguage::SimplifiedChinese,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StoredTheme {
    #[default]
    System,
    Light,
    Dark,
}

impl StoredTheme {
    fn application_value(self) -> ApplicationTheme {
        match self {
            Self::System => ApplicationTheme::System,
            Self::Light => ApplicationTheme::Light,
            Self::Dark => ApplicationTheme::Dark,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StoredCloseBehavior {
    #[default]
    Quit,
    MinimizeToTray,
}

impl StoredCloseBehavior {
    fn application_value(self) -> CloseBehavior {
        match self {
            Self::Quit => CloseBehavior::Quit,
            Self::MinimizeToTray => CloseBehavior::MinimizeToTray,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredAppearance {
    language: StoredLanguage,
    theme: StoredTheme,
}

impl Default for StoredAppearance {
    fn default() -> Self {
        Self {
            language: StoredLanguage::System,
            theme: StoredTheme::System,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredBehavior {
    close_behavior: StoredCloseBehavior,
    background_operations_enabled: bool,
}

impl Default for StoredBehavior {
    fn default() -> Self {
        Self {
            close_behavior: StoredCloseBehavior::Quit,
            background_operations_enabled: true,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredApplicationConfig {
    schema_version: u16,
    appearance: StoredAppearance,
    behavior: StoredBehavior,
}

impl Default for StoredApplicationConfig {
    fn default() -> Self {
        Self {
            schema_version: APP_CONFIG_SCHEMA_VERSION,
            appearance: StoredAppearance::default(),
            behavior: StoredBehavior::default(),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ApplicationConfigStore {
    path: PathBuf,
    owner: Option<FileOwner>,
}

impl ApplicationConfigStore {
    pub(crate) fn from_environment() -> Result<Self, ApplicationConfigError> {
        let (root, owner) = app_config_location()?;
        Ok(Self {
            path: root.join("config.json"),
            owner,
        })
    }

    #[cfg(test)]
    pub(crate) fn at(path: PathBuf) -> Self {
        Self { path, owner: None }
    }

    fn load(&self) -> Result<StoredApplicationConfig, ApplicationConfigError> {
        read_json::<StoredApplicationConfig>(&self.path)?
            .map_or_else(|| Ok(StoredApplicationConfig::default()), validate_schema)
    }

    pub(crate) fn load_preferences(
        &self,
    ) -> Result<ApplicationPreferences, ApplicationConfigError> {
        let config = self.load()?;
        Ok(ApplicationPreferences {
            language: config.appearance.language.application_value(),
            theme: config.appearance.theme.application_value(),
            close_behavior: config.behavior.close_behavior.application_value(),
            background_operations_enabled: config.behavior.background_operations_enabled,
        })
    }

    pub(crate) fn save_preference(
        &self,
        change: ApplicationPreferenceChange,
    ) -> Result<(), ApplicationConfigError> {
        match change {
            ApplicationPreferenceChange::Language(language) => self.save_language(language.into()),
            ApplicationPreferenceChange::Theme(theme) => self.save_theme(theme.into()),
            ApplicationPreferenceChange::CloseBehavior(behavior) => {
                self.save_close_behavior(behavior.into())
            }
            ApplicationPreferenceChange::BackgroundOperationsEnabled(enabled) => {
                self.save_background_operations_enabled(enabled)
            }
        }
    }

    fn save_language(&self, language: StoredLanguage) -> Result<(), ApplicationConfigError> {
        self.update(|config| config.appearance.language = language)
    }

    fn save_theme(&self, theme: StoredTheme) -> Result<(), ApplicationConfigError> {
        self.update(|config| config.appearance.theme = theme)
    }

    fn save_close_behavior(
        &self,
        close_behavior: StoredCloseBehavior,
    ) -> Result<(), ApplicationConfigError> {
        self.update(|config| config.behavior.close_behavior = close_behavior)
    }

    fn save_background_operations_enabled(
        &self,
        enabled: bool,
    ) -> Result<(), ApplicationConfigError> {
        self.update(|config| config.behavior.background_operations_enabled = enabled)
    }

    fn update(
        &self,
        mutate: impl FnOnce(&mut StoredApplicationConfig),
    ) -> Result<(), ApplicationConfigError> {
        let mut config = self.load()?;
        config.schema_version = APP_CONFIG_SCHEMA_VERSION;
        mutate(&mut config);
        self.save(&config)
    }

    fn save(&self, config: &StoredApplicationConfig) -> Result<(), ApplicationConfigError> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| ApplicationConfigError::InvalidPath {
                path: self.path.clone(),
            })?;
        create_config_directory(parent, self.owner)?;

        let temporary_path = temporary_path(&self.path);
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temporary_path)
            .map_err(|source| ApplicationConfigError::Write {
                path: temporary_path.clone(),
                source,
            })?;
        set_owner(&file, self.owner, &temporary_path)?;
        let mut writer = BufWriter::new(file);
        serde_json::to_writer_pretty(&mut writer, config).map_err(|source| {
            ApplicationConfigError::Encode {
                path: temporary_path.clone(),
                source,
            }
        })?;
        writer
            .write_all(b"\n")
            .and_then(|()| writer.flush())
            .map_err(|source| ApplicationConfigError::Write {
                path: temporary_path.clone(),
                source,
            })?;
        writer
            .into_inner()
            .map_err(|error| ApplicationConfigError::Write {
                path: temporary_path.clone(),
                source: error.into_error(),
            })?
            .sync_all()
            .map_err(|source| ApplicationConfigError::Write {
                path: temporary_path.clone(),
                source,
            })?;
        fs::rename(&temporary_path, &self.path).map_err(|source| {
            ApplicationConfigError::Write {
                path: self.path.clone(),
                source,
            }
        })?;
        sync_directory(parent);
        Ok(())
    }
}

impl From<ApplicationLanguage> for StoredLanguage {
    fn from(language: ApplicationLanguage) -> Self {
        match language {
            ApplicationLanguage::System => Self::System,
            ApplicationLanguage::English => Self::English,
            ApplicationLanguage::SimplifiedChinese => Self::SimplifiedChinese,
        }
    }
}

impl From<ApplicationTheme> for StoredTheme {
    fn from(theme: ApplicationTheme) -> Self {
        match theme {
            ApplicationTheme::System => Self::System,
            ApplicationTheme::Light => Self::Light,
            ApplicationTheme::Dark => Self::Dark,
        }
    }
}

impl From<CloseBehavior> for StoredCloseBehavior {
    fn from(behavior: CloseBehavior) -> Self {
        match behavior {
            CloseBehavior::Quit => Self::Quit,
            CloseBehavior::MinimizeToTray => Self::MinimizeToTray,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct FileOwner {
    uid: u32,
    gid: u32,
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, ApplicationConfigError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(ApplicationConfigError::Read {
                path: path.to_owned(),
                source,
            });
        }
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|source| ApplicationConfigError::Decode {
            path: path.to_owned(),
            source,
        })
}

fn validate_schema(
    config: StoredApplicationConfig,
) -> Result<StoredApplicationConfig, ApplicationConfigError> {
    if config.schema_version != APP_CONFIG_SCHEMA_VERSION {
        return Err(ApplicationConfigError::UnsupportedSchemaVersion {
            found: config.schema_version,
            supported: APP_CONFIG_SCHEMA_VERSION,
        });
    }
    Ok(config)
}

fn app_config_location() -> Result<(PathBuf, Option<FileOwner>), ApplicationConfigError> {
    if let Some(context) = desktop::elevated_user()
        && let (Some(uid), Some(gid)) = (context.uid, context.gid)
    {
        return Ok((
            context.home.join(".config").join("dogi"),
            Some(FileOwner { uid, gid }),
        ));
    }
    if let Some(path) = non_empty_env_path("XDG_CONFIG_HOME") {
        return Ok((path.join("dogi"), None));
    }
    non_empty_env_path("HOME")
        .map(|home| home.join(".config").join("dogi"))
        .map(|path| (path, None))
        .ok_or(ApplicationConfigError::ConfigDirectoryUnavailable)
}

fn non_empty_env_path(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn temporary_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.json");
    path.with_file_name(format!(".{file_name}.tmp-{}", std::process::id()))
}

fn create_config_directory(
    path: &Path,
    owner: Option<FileOwner>,
) -> Result<(), ApplicationConfigError> {
    let mut missing = Vec::new();
    let mut current = Some(path);
    while let Some(candidate) = current {
        if candidate.exists() {
            break;
        }
        missing.push(candidate.to_owned());
        current = candidate.parent();
    }

    fs::create_dir_all(path).map_err(|source| ApplicationConfigError::Write {
        path: path.to_owned(),
        source,
    })?;
    for directory in missing.iter().rev() {
        let file = File::open(directory).map_err(|source| ApplicationConfigError::Write {
            path: directory.clone(),
            source,
        })?;
        set_owner(&file, owner, directory)?;
    }

    let file = File::open(path).map_err(|source| ApplicationConfigError::Write {
        path: path.to_owned(),
        source,
    })?;
    set_owner(&file, owner, path)
}

#[cfg(unix)]
fn set_owner(
    file: &File,
    owner: Option<FileOwner>,
    path: &Path,
) -> Result<(), ApplicationConfigError> {
    let Some(owner) = owner else {
        return Ok(());
    };
    let result = unsafe { libc::fchown(file.as_raw_fd(), owner.uid, owner.gid) };
    if result == 0 {
        Ok(())
    } else {
        Err(ApplicationConfigError::Ownership {
            path: path.to_owned(),
            source: io::Error::last_os_error(),
        })
    }
}

#[cfg(not(unix))]
fn set_owner(
    _file: &File,
    _owner: Option<FileOwner>,
    _path: &Path,
) -> Result<(), ApplicationConfigError> {
    Ok(())
}

fn sync_directory(path: &Path) {
    if let Ok(directory) = File::open(path) {
        let _ = directory.sync_all();
    }
}

#[derive(Debug)]
pub(crate) enum ApplicationConfigError {
    ConfigDirectoryUnavailable,
    InvalidPath {
        path: PathBuf,
    },
    Read {
        path: PathBuf,
        source: io::Error,
    },
    Decode {
        path: PathBuf,
        source: serde_json::Error,
    },
    Encode {
        path: PathBuf,
        source: serde_json::Error,
    },
    Write {
        path: PathBuf,
        source: io::Error,
    },
    Ownership {
        path: PathBuf,
        source: io::Error,
    },
    UnsupportedSchemaVersion {
        found: u16,
        supported: u16,
    },
}

impl fmt::Display for ApplicationConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConfigDirectoryUnavailable => {
                formatter.write_str("HOME or XDG_CONFIG_HOME is not set")
            }
            Self::InvalidPath { path } => write!(formatter, "invalid path {}", path.display()),
            Self::Read { path, source } => {
                write!(formatter, "could not read {}: {source}", path.display())
            }
            Self::Decode { path, source } => {
                write!(formatter, "could not decode {}: {source}", path.display())
            }
            Self::Encode { path, source } => {
                write!(formatter, "could not encode {}: {source}", path.display())
            }
            Self::Write { path, source } => {
                write!(formatter, "could not write {}: {source}", path.display())
            }
            Self::Ownership { path, source } => {
                write!(
                    formatter,
                    "could not preserve ownership of {}: {source}",
                    path.display()
                )
            }
            Self::UnsupportedSchemaVersion { found, supported } => write!(
                formatter,
                "unsupported application config schema {found}; expected {supported}"
            ),
        }
    }
}

impl std::error::Error for ApplicationConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read { source, .. }
            | Self::Write { source, .. }
            | Self::Ownership { source, .. } => Some(source),
            Self::Decode { source, .. } | Self::Encode { source, .. } => Some(source),
            Self::ConfigDirectoryUnavailable
            | Self::InvalidPath { .. }
            | Self::UnsupportedSchemaVersion { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_config_round_trip_preserves_the_versioned_schema() {
        let root = unique_test_root("roundtrip");
        let store = ApplicationConfigStore::at(root.join("config.json"));

        store
            .save_language(StoredLanguage::SimplifiedChinese)
            .unwrap();
        assert_eq!(
            store.load().unwrap(),
            StoredApplicationConfig {
                schema_version: APP_CONFIG_SCHEMA_VERSION,
                appearance: StoredAppearance {
                    language: StoredLanguage::SimplifiedChinese,
                    theme: StoredTheme::System,
                },
                behavior: StoredBehavior::default(),
            }
        );
        let json = fs::read_to_string(root.join("config.json")).unwrap();
        assert!(json.contains("\"schema_version\": 1"));
        assert!(json.contains("\"appearance\""));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn missing_app_config_uses_system_language() {
        let root = unique_test_root("missing");
        let store = ApplicationConfigStore::at(root.join("config.json"));

        assert_eq!(store.load().unwrap(), StoredApplicationConfig::default());
    }

    #[test]
    fn appearance_and_behavior_updates_preserve_each_other() {
        let root = unique_test_root("settings");
        let store = ApplicationConfigStore::at(root.join("config.json"));

        store.save_theme(StoredTheme::Dark).unwrap();
        store
            .save_close_behavior(StoredCloseBehavior::MinimizeToTray)
            .unwrap();
        store.save_language(StoredLanguage::English).unwrap();

        let config = store.load().unwrap();
        assert_eq!(config.appearance.language, StoredLanguage::English);
        assert_eq!(config.appearance.theme, StoredTheme::Dark);
        assert_eq!(
            config.behavior.close_behavior,
            StoredCloseBehavior::MinimizeToTray
        );
        assert!(config.behavior.background_operations_enabled);

        store.save_background_operations_enabled(false).unwrap();
        let config = store.load().unwrap();
        assert!(!config.behavior.background_operations_enabled);
        assert_eq!(
            config.behavior.close_behavior,
            StoredCloseBehavior::MinimizeToTray
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn preference_port_round_trips_presentation_values() {
        let root = unique_test_root("preference-port");
        let store = ApplicationConfigStore::at(root.join("config.json"));

        store
            .save_preference(ApplicationPreferenceChange::Language(
                ApplicationLanguage::SimplifiedChinese,
            ))
            .unwrap();
        store
            .save_preference(ApplicationPreferenceChange::Theme(ApplicationTheme::Dark))
            .unwrap();
        store
            .save_preference(ApplicationPreferenceChange::CloseBehavior(
                CloseBehavior::MinimizeToTray,
            ))
            .unwrap();
        store
            .save_preference(ApplicationPreferenceChange::BackgroundOperationsEnabled(
                false,
            ))
            .unwrap();

        assert_eq!(
            store.load_preferences().unwrap(),
            ApplicationPreferences {
                language: ApplicationLanguage::SimplifiedChinese,
                theme: ApplicationTheme::Dark,
                close_behavior: CloseBehavior::MinimizeToTray,
                background_operations_enabled: false,
            }
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn incomplete_current_schema_is_rejected() {
        let root = unique_test_root("incomplete-schema");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("config.json"),
            r#"{"schema_version":1,"appearance":{"language":"simplified_chinese"}}"#,
        )
        .unwrap();
        let store = ApplicationConfigStore::at(root.join("config.json"));

        assert!(matches!(
            store.load(),
            Err(ApplicationConfigError::Decode { .. })
        ));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn unsupported_app_config_schema_is_not_overwritten() {
        let root = unique_test_root("unsupported-schema");
        let store = ApplicationConfigStore::at(root.join("config.json"));
        store
            .save(&StoredApplicationConfig {
                schema_version: APP_CONFIG_SCHEMA_VERSION + 1,
                ..StoredApplicationConfig::default()
            })
            .unwrap();

        assert!(matches!(
            store.load(),
            Err(ApplicationConfigError::UnsupportedSchemaVersion { .. })
        ));

        let _ = fs::remove_dir_all(root);
    }

    fn unique_test_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "dogi-app-config-{label}-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("unnamed")
        ))
    }
}

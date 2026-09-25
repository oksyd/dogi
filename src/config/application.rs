use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::ui::{
    ApplicationLanguage, ApplicationPreferenceChange, ApplicationPreferences, ApplicationTheme,
    CloseBehavior, NetworkProxyMode, NetworkProxyPreferences, NetworkProxyProtocol,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::environment::AppEnvironment;
use crate::persistence::{
    ExclusiveFileLock, FileOwner, PersistenceError, atomic_write, quarantine, sibling_lock_path,
};

const APP_CONFIG_SCHEMA_VERSION: u16 = 6;

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
    #[serde(default = "enabled_by_default")]
    low_battery_notifications_enabled: bool,
    #[serde(default = "enabled_by_default")]
    full_battery_notifications_enabled: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredUpdates {
    automatic_update_checks_enabled: bool,
    #[serde(default)]
    last_successful_check_unix_seconds: Option<u64>,
}

impl Default for StoredUpdates {
    fn default() -> Self {
        Self {
            automatic_update_checks_enabled: true,
            last_successful_check_unix_seconds: None,
        }
    }
}

impl Default for StoredBehavior {
    fn default() -> Self {
        Self {
            close_behavior: StoredCloseBehavior::Quit,
            background_operations_enabled: true,
            low_battery_notifications_enabled: true,
            full_battery_notifications_enabled: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StoredProxyMode {
    #[default]
    System,
    Direct,
    Manual,
}

impl StoredProxyMode {
    fn application_value(self) -> NetworkProxyMode {
        match self {
            Self::System => NetworkProxyMode::System,
            Self::Direct => NetworkProxyMode::Direct,
            Self::Manual => NetworkProxyMode::Manual,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StoredProxyProtocol {
    #[default]
    Http,
    Https,
    Socks5,
}

impl StoredProxyProtocol {
    fn application_value(self) -> NetworkProxyProtocol {
        match self {
            Self::Http => NetworkProxyProtocol::Http,
            Self::Https => NetworkProxyProtocol::Https,
            Self::Socks5 => NetworkProxyProtocol::Socks5,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredManualProxy {
    protocol: StoredProxyProtocol,
    host: String,
    port: u16,
    authentication_enabled: bool,
    username: String,
    password_saved: bool,
}

impl Default for StoredManualProxy {
    fn default() -> Self {
        Self {
            protocol: StoredProxyProtocol::Http,
            host: String::new(),
            port: 7890,
            authentication_enabled: false,
            username: String::new(),
            password_saved: false,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredProxy {
    mode: StoredProxyMode,
    manual: StoredManualProxy,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredNetwork {
    proxy: StoredProxy,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredApplicationConfig {
    schema_version: u16,
    appearance: StoredAppearance,
    behavior: StoredBehavior,
    #[serde(default)]
    updates: StoredUpdates,
    #[serde(default)]
    network: StoredNetwork,
}

impl Default for StoredApplicationConfig {
    fn default() -> Self {
        Self {
            schema_version: APP_CONFIG_SCHEMA_VERSION,
            appearance: StoredAppearance::default(),
            behavior: StoredBehavior::default(),
            updates: StoredUpdates::default(),
            network: StoredNetwork::default(),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ApplicationConfigStore {
    path: PathBuf,
    lock_path: PathBuf,
    owner: Option<FileOwner>,
    defaults: StoredApplicationConfig,
    write_lock: Arc<Mutex<()>>,
    recovery_notice: Arc<Mutex<Option<String>>>,
}

impl ApplicationConfigStore {
    pub(crate) fn for_environment(environment: &AppEnvironment) -> Self {
        let owner = environment
            .user
            .uid
            .zip(environment.user.gid)
            .map(|(uid, gid)| FileOwner::new(uid, gid));
        let mut defaults = StoredApplicationConfig::default();
        defaults.behavior.background_operations_enabled =
            environment.default_background_operations_enabled();
        defaults.updates.automatic_update_checks_enabled = environment.updates.enabled;
        let path = environment.paths.application_config();
        Self {
            lock_path: sibling_lock_path(&path),
            path,
            owner,
            defaults,
            write_lock: Arc::new(Mutex::new(())),
            recovery_notice: Arc::default(),
        }
    }

    #[cfg(test)]
    pub(crate) fn at(path: PathBuf) -> Self {
        Self {
            lock_path: sibling_lock_path(&path),
            path,
            owner: None,
            defaults: StoredApplicationConfig::default(),
            write_lock: Arc::new(Mutex::new(())),
            recovery_notice: Arc::default(),
        }
    }

    fn load(&self) -> Result<StoredApplicationConfig, ApplicationConfigError> {
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _file_lock = ExclusiveFileLock::acquire(&self.lock_path, self.owner)?;
        self.load_locked()
    }

    fn load_locked(&self) -> Result<StoredApplicationConfig, ApplicationConfigError> {
        let loaded = read_json::<StoredApplicationConfig>(&self.path)
            .and_then(|config| config.map_or_else(|| Ok(self.defaults.clone()), validate_schema));
        match loaded {
            Ok(config) => Ok(config),
            Err(error) if error.recoverable() => self.recover_invalid_config(error.to_string()),
            Err(error) => Err(error),
        }
    }

    fn recover_invalid_config(
        &self,
        reason: String,
    ) -> Result<StoredApplicationConfig, ApplicationConfigError> {
        let backup = quarantine(&self.path, "incompatible-config")?;
        let defaults = self.defaults.clone();
        self.save(&defaults)?;
        let backup_detail = backup
            .as_deref()
            .map(|path| format!(" The original was preserved at {}.", path.display()))
            .unwrap_or_default();
        *self
            .recovery_notice
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(format!(
            "Application settings were restored to defaults because {reason}.{backup_detail}"
        ));
        Ok(defaults)
    }

    pub(crate) fn take_recovery_notice(&self) -> Option<String> {
        self.recovery_notice
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
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
            low_battery_notifications_enabled: config.behavior.low_battery_notifications_enabled,
            full_battery_notifications_enabled: config.behavior.full_battery_notifications_enabled,
            automatic_update_checks_enabled: config.updates.automatic_update_checks_enabled,
        })
    }

    pub(crate) fn default_preferences(&self) -> ApplicationPreferences {
        ApplicationPreferences {
            language: self.defaults.appearance.language.application_value(),
            theme: self.defaults.appearance.theme.application_value(),
            close_behavior: self.defaults.behavior.close_behavior.application_value(),
            background_operations_enabled: self.defaults.behavior.background_operations_enabled,
            low_battery_notifications_enabled: self
                .defaults
                .behavior
                .low_battery_notifications_enabled,
            full_battery_notifications_enabled: self
                .defaults
                .behavior
                .full_battery_notifications_enabled,
            automatic_update_checks_enabled: self.defaults.updates.automatic_update_checks_enabled,
        }
    }

    pub(crate) fn load_network_proxy(
        &self,
    ) -> Result<NetworkProxyPreferences, ApplicationConfigError> {
        let proxy = self.load()?.network.proxy;
        Ok(NetworkProxyPreferences {
            mode: proxy.mode.application_value(),
            protocol: proxy.manual.protocol.application_value(),
            host: proxy.manual.host,
            port: proxy.manual.port,
            authentication_enabled: proxy.manual.authentication_enabled,
            username: proxy.manual.username,
            password_saved: proxy.manual.password_saved,
        })
    }

    pub(crate) fn default_network_proxy(&self) -> NetworkProxyPreferences {
        NetworkProxyPreferences::default()
    }

    pub(crate) fn save_network_proxy(
        &self,
        preferences: &NetworkProxyPreferences,
    ) -> Result<(), ApplicationConfigError> {
        let stored = StoredProxy {
            mode: preferences.mode.into(),
            manual: StoredManualProxy {
                protocol: preferences.protocol.into(),
                host: preferences.host.clone(),
                port: preferences.port,
                authentication_enabled: preferences.authentication_enabled,
                username: preferences.username.clone(),
                password_saved: preferences.password_saved,
            },
        };
        self.update(|config| config.network.proxy = stored)
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
            ApplicationPreferenceChange::LowBatteryNotificationsEnabled(enabled) => {
                self.save_low_battery_notifications_enabled(enabled)
            }
            ApplicationPreferenceChange::FullBatteryNotificationsEnabled(enabled) => {
                self.save_full_battery_notifications_enabled(enabled)
            }
            ApplicationPreferenceChange::AutomaticUpdateChecksEnabled(enabled) => {
                self.save_automatic_update_checks_enabled(enabled)
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

    fn save_low_battery_notifications_enabled(
        &self,
        enabled: bool,
    ) -> Result<(), ApplicationConfigError> {
        self.update(|config| config.behavior.low_battery_notifications_enabled = enabled)
    }

    fn save_full_battery_notifications_enabled(
        &self,
        enabled: bool,
    ) -> Result<(), ApplicationConfigError> {
        self.update(|config| config.behavior.full_battery_notifications_enabled = enabled)
    }

    fn save_automatic_update_checks_enabled(
        &self,
        enabled: bool,
    ) -> Result<(), ApplicationConfigError> {
        self.update(|config| config.updates.automatic_update_checks_enabled = enabled)
    }

    pub(crate) fn automatic_update_check_due(
        &self,
        now: SystemTime,
        interval: Duration,
    ) -> Result<bool, ApplicationConfigError> {
        let Some(last_check) = self.load()?.updates.last_successful_check_unix_seconds else {
            return Ok(true);
        };
        let now = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        Ok(now.saturating_sub(last_check) >= interval.as_secs())
    }

    pub(crate) fn record_successful_update_check(
        &self,
        checked_at: SystemTime,
    ) -> Result<(), ApplicationConfigError> {
        let seconds = checked_at
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.update(|config| {
            config.updates.last_successful_check_unix_seconds = Some(seconds);
        })
    }

    fn update(
        &self,
        mutate: impl FnOnce(&mut StoredApplicationConfig),
    ) -> Result<(), ApplicationConfigError> {
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _file_lock = ExclusiveFileLock::acquire(&self.lock_path, self.owner)?;
        let mut config = self.load_locked()?;
        mutate(&mut config);
        self.save(&config)
    }

    fn save(&self, config: &StoredApplicationConfig) -> Result<(), ApplicationConfigError> {
        let mut encoded =
            serde_json::to_vec_pretty(config).map_err(|source| ApplicationConfigError::Encode {
                path: self.path.clone(),
                source,
            })?;
        encoded.push(b'\n');
        atomic_write(&self.path, &encoded, self.owner).map_err(ApplicationConfigError::from)
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

impl From<NetworkProxyMode> for StoredProxyMode {
    fn from(mode: NetworkProxyMode) -> Self {
        match mode {
            NetworkProxyMode::System => Self::System,
            NetworkProxyMode::Direct => Self::Direct,
            NetworkProxyMode::Manual => Self::Manual,
        }
    }
}

impl From<NetworkProxyProtocol> for StoredProxyProtocol {
    fn from(protocol: NetworkProxyProtocol) -> Self {
        match protocol {
            NetworkProxyProtocol::Http => Self::Http,
            NetworkProxyProtocol::Https => Self::Https,
            NetworkProxyProtocol::Socks5 => Self::Socks5,
        }
    }
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

const fn enabled_by_default() -> bool {
    true
}

#[derive(Debug)]
pub(crate) enum ApplicationConfigError {
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
    Persistence(PersistenceError),
    UnsupportedSchemaVersion {
        found: u16,
        supported: u16,
    },
}

impl fmt::Display for ApplicationConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(formatter, "could not read {}: {source}", path.display())
            }
            Self::Decode { path, source } => {
                write!(formatter, "could not decode {}: {source}", path.display())
            }
            Self::Encode { path, source } => {
                write!(formatter, "could not encode {}: {source}", path.display())
            }
            Self::Persistence(error) => error.fmt(formatter),
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
            Self::Read { source, .. } => Some(source),
            Self::Decode { source, .. } | Self::Encode { source, .. } => Some(source),
            Self::Persistence(error) => Some(error),
            Self::UnsupportedSchemaVersion { .. } => None,
        }
    }
}

impl ApplicationConfigError {
    fn recoverable(&self) -> bool {
        matches!(
            self,
            Self::Decode { .. } | Self::UnsupportedSchemaVersion { .. }
        )
    }
}

impl From<PersistenceError> for ApplicationConfigError {
    fn from(error: PersistenceError) -> Self {
        Self::Persistence(error)
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
                updates: StoredUpdates::default(),
                network: StoredNetwork::default(),
            }
        );
        let json = fs::read_to_string(root.join("config.json")).unwrap();
        assert!(json.contains("\"schema_version\": 6"));
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
        store
            .save_preference(ApplicationPreferenceChange::LowBatteryNotificationsEnabled(
                false,
            ))
            .unwrap();
        store
            .save_preference(ApplicationPreferenceChange::FullBatteryNotificationsEnabled(false))
            .unwrap();
        store
            .save_preference(ApplicationPreferenceChange::AutomaticUpdateChecksEnabled(
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
                low_battery_notifications_enabled: false,
                full_battery_notifications_enabled: false,
                automatic_update_checks_enabled: false,
            }
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn incompatible_schema_is_preserved_and_replaced_with_defaults() {
        let root = unique_test_root("incompatible-schema");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("config.json"),
            r#"{
                "schema_version": 1,
                "appearance": {"language": "english", "theme": "dark"},
                "behavior": {"close_behavior": "quit", "background_operations_enabled": true}
            }"#,
        )
        .unwrap();
        let store = ApplicationConfigStore::at(root.join("config.json"));

        let config = store.load().unwrap();
        assert_eq!(config, StoredApplicationConfig::default());
        assert!(store.take_recovery_notice().is_some_and(|notice| {
            notice.contains("restored to defaults") && notice.contains("preserved")
        }));
        let files = fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(
            files
                .iter()
                .any(|name| name.contains("incompatible-config"))
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn successful_update_checks_are_throttled_for_the_configured_interval() {
        let root = unique_test_root("update-check-throttle");
        let store = ApplicationConfigStore::at(root.join("config.json"));
        let checked_at = UNIX_EPOCH + Duration::from_secs(1_000_000);
        let interval = Duration::from_secs(24 * 60 * 60);

        assert!(
            store
                .automatic_update_check_due(checked_at, interval)
                .unwrap()
        );
        store.record_successful_update_check(checked_at).unwrap();
        assert!(
            !store
                .automatic_update_check_due(
                    checked_at + interval - Duration::from_secs(1),
                    interval
                )
                .unwrap()
        );
        assert!(
            store
                .automatic_update_check_due(checked_at + interval, interval)
                .unwrap()
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn network_proxy_round_trip_preserves_manual_settings_without_a_secret() {
        let root = unique_test_root("network-proxy");
        let store = ApplicationConfigStore::at(root.join("config.json"));
        let preferences = NetworkProxyPreferences {
            mode: NetworkProxyMode::Manual,
            protocol: NetworkProxyProtocol::Socks5,
            host: "127.0.0.1".to_owned(),
            port: 7890,
            authentication_enabled: true,
            username: "proxy-user".to_owned(),
            password_saved: true,
        };

        store.save_network_proxy(&preferences).unwrap();

        assert_eq!(store.load_network_proxy().unwrap(), preferences);
        let json = fs::read_to_string(root.join("config.json")).unwrap();
        assert!(!json.contains("password\":"));
        assert!(!json.contains("proxy-password"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn malformed_config_is_preserved_and_replaced_with_defaults() {
        let root = unique_test_root("incomplete-schema");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("config.json"),
            r#"{"schema_version":1,"appearance":{"language":"simplified_chinese"}}"#,
        )
        .unwrap();
        let store = ApplicationConfigStore::at(root.join("config.json"));

        assert_eq!(store.load().unwrap(), StoredApplicationConfig::default());
        assert!(store.take_recovery_notice().is_some());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn future_app_config_schema_is_preserved_and_replaced_with_defaults() {
        let root = unique_test_root("unsupported-schema");
        let store = ApplicationConfigStore::at(root.join("config.json"));
        store
            .save(&StoredApplicationConfig {
                schema_version: APP_CONFIG_SCHEMA_VERSION + 1,
                ..StoredApplicationConfig::default()
            })
            .unwrap();

        assert_eq!(store.load().unwrap(), StoredApplicationConfig::default());
        assert!(store.take_recovery_notice().is_some());

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

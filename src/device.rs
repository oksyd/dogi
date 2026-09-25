use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::domain::{
    ActiveApplication, DeviceInfo, DogiError, LocalRuntimePlan, Master3sSettings,
    ResolvedRuntimeAction, Result, SettingsApplyPlan, SettingsApplyPreview, SettingsApplyReport,
    SettingsTransactionState,
};
use serde::{Deserialize, Serialize};

use crate::domain::Master3sRuntimeEvent;
use crate::hid::{Master3sRuntimeEventListener, PreparedSettingsTransaction};

use crate::desktop::focus;
use crate::environment::AppEnvironment;
use crate::persistence::{
    ExclusiveFileLock, FileOwner, atomic_write, durable_remove, quarantine, sibling_lock_path,
};
use crate::runtime::{self, RuntimeActionExecution};

#[derive(Clone, Debug, Default)]
pub(crate) struct DeviceService {
    config_path: Option<PathBuf>,
    config_owner: Option<FileOwner>,
    transaction_dir: Option<PathBuf>,
    legacy_transaction_path: Option<PathBuf>,
    pending: Arc<Mutex<Option<PendingSettingsTransaction>>>,
    recovery_notice: Arc<Mutex<Option<String>>>,
}

#[derive(Clone, Debug)]
struct PendingSettingsTransaction {
    device_id: String,
    settings: Master3sSettings,
    plan: SettingsApplyPlan,
    transaction: PreparedSettingsTransaction,
}

const SETTINGS_FILE_VERSION: u8 = 5;
const DEVICE_TRANSACTION_FILE_VERSION: u8 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DeviceTransactionPhase {
    Prepared,
    DeviceCommitted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecoveryMode {
    Gui,
    Runtime,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredDeviceTransaction {
    version: u8,
    phase: DeviceTransactionPhase,
    transaction: PreparedSettingsTransaction,
    settings_id: Option<String>,
    settings: Option<Master3sSettings>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredMaster3sSettings {
    version: u8,
    default: Master3sSettings,
    devices: BTreeMap<String, Master3sSettings>,
}

impl Default for StoredMaster3sSettings {
    fn default() -> Self {
        Self {
            version: SETTINGS_FILE_VERSION,
            default: Master3sSettings::default(),
            devices: BTreeMap::new(),
        }
    }
}

impl StoredMaster3sSettings {
    fn normalized(&self) -> Self {
        Self {
            version: SETTINGS_FILE_VERSION,
            default: self.default.normalized(),
            devices: self
                .devices
                .iter()
                .filter(|(device_id, _)| !device_id.trim().is_empty())
                .map(|(device_id, settings)| (device_id.clone(), settings.normalized()))
                .collect(),
        }
    }

    fn validated(self) -> Result<Self> {
        if self.version != SETTINGS_FILE_VERSION {
            return Err(DogiError::Config(format!(
                "unsupported settings schema version {}; expected {}",
                self.version, SETTINGS_FILE_VERSION
            )));
        }
        Ok(self.normalized())
    }
}

impl DeviceService {
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn for_environment(environment: &AppEnvironment) -> Self {
        let owner = environment
            .user
            .uid
            .zip(environment.user.gid)
            .map(|(uid, gid)| FileOwner::new(uid, gid));
        Self {
            config_path: Some(environment.paths.device_settings()),
            config_owner: owner,
            transaction_dir: Some(environment.paths.device_transactions_dir()),
            legacy_transaction_path: Some(environment.paths.device_transaction()),
            pending: Arc::default(),
            recovery_notice: Arc::default(),
        }
    }

    #[cfg(test)]
    pub fn with_config_path(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let transaction_dir = path.with_extension("transactions");
        Self {
            config_path: Some(path),
            config_owner: None,
            transaction_dir: Some(transaction_dir),
            legacy_transaction_path: None,
            pending: Arc::default(),
            recovery_notice: Arc::default(),
        }
    }

    pub fn scan_devices(&self) -> Result<Vec<DeviceInfo>> {
        crate::hid::scan_devices()
    }

    pub fn scan_device_inventory(&self) -> Result<Vec<DeviceInfo>> {
        crate::hid::scan_device_inventory()
    }

    pub fn scan_devices_for_ui(&self) -> Result<Vec<DeviceInfo>> {
        crate::hid::scan_devices_for_ui()
    }

    pub fn scan_all_devices(&self) -> Result<Vec<DeviceInfo>> {
        crate::hid::scan_all_devices()
    }

    pub fn find_device(&self, id: &str) -> Result<DeviceInfo> {
        crate::hid::find_device(id)
    }

    pub fn plan_master3s_settings(
        &self,
        device_id: &str,
        settings: &Master3sSettings,
    ) -> SettingsApplyPlan {
        crate::domain::build_master3s_apply_plan(device_id, &settings.normalized())
    }

    pub fn plan_master3s_runtime(&self, settings: &Master3sSettings) -> LocalRuntimePlan {
        crate::domain::build_master3s_runtime_plan(&settings.normalized())
    }

    pub fn apply_master3s_settings(
        &self,
        device_id: &str,
        settings: &Master3sSettings,
    ) -> Result<SettingsApplyReport> {
        let settings = settings.normalized();
        let plan = self.plan_master3s_settings(device_id, &settings);
        self.apply_master3s_settings_plan(device_id, &settings, &plan)
    }

    pub fn apply_master3s_settings_plan(
        &self,
        device_id: &str,
        settings: &Master3sSettings,
        plan: &SettingsApplyPlan,
    ) -> Result<SettingsApplyReport> {
        let candidate =
            crate::hid::prepare_master3s_settings_plan(device_id, &settings.normalized(), plan)?;
        let transaction_path = self.transaction_path_for(&candidate)?;
        let _lock = self.acquire_transaction_lock(&transaction_path)?;
        self.ensure_interrupted_transaction_recovered_locked(&transaction_path)?;
        let transaction =
            crate::hid::prepare_master3s_settings_plan(device_id, &settings.normalized(), plan)?;
        if self.transaction_path_for(&transaction)? != transaction_path {
            return Err(DogiError::Protocol(
                "device identity changed while preparing the settings transaction".to_owned(),
            ));
        }
        let journal = StoredDeviceTransaction {
            version: DEVICE_TRANSACTION_FILE_VERSION,
            phase: DeviceTransactionPhase::Prepared,
            transaction: transaction.clone(),
            settings_id: None,
            settings: None,
        };
        self.write_device_transaction(&transaction_path, &journal)?;
        let report = match crate::hid::execute_prepared_master3s_settings_transaction(&transaction)
        {
            Ok(report) => report,
            Err(error) => {
                self.clear_device_transaction(&transaction_path)?;
                return Err(error);
            }
        };
        if report.transaction != SettingsTransactionState::RecoveryRequired {
            self.clear_device_transaction(&transaction_path)?;
        }
        Ok(report)
    }

    pub fn prepare_master3s_settings_transaction(
        &self,
        device_id: &str,
        settings: &Master3sSettings,
        plan: &SettingsApplyPlan,
    ) -> Result<SettingsApplyPreview> {
        let settings = settings.normalized();
        let candidate = crate::hid::prepare_master3s_settings_plan(device_id, &settings, plan)?;
        let transaction_path = self.transaction_path_for(&candidate)?;
        let _lock = self.acquire_transaction_lock(&transaction_path)?;
        self.ensure_interrupted_transaction_recovered_locked(&transaction_path)?;
        let transaction = crate::hid::prepare_master3s_settings_plan(device_id, &settings, plan)?;
        if self.transaction_path_for(&transaction)? != transaction_path {
            return Err(DogiError::Protocol(
                "device identity changed while preparing the settings preview".to_owned(),
            ));
        }
        let preview = transaction.preview();
        *self.pending.lock().map_err(pending_lock_error)? = Some(PendingSettingsTransaction {
            device_id: device_id.to_owned(),
            settings,
            plan: plan.clone(),
            transaction,
        });
        Ok(preview)
    }

    pub fn commit_prepared_master3s_settings(
        &self,
        device_id: &str,
        settings_id: &str,
        settings: &Master3sSettings,
        plan: &SettingsApplyPlan,
    ) -> Result<(SettingsApplyReport, PathBuf)> {
        let settings_id = settings_id.trim();
        if settings_id.is_empty() {
            return Err(DogiError::InvalidArgument(
                "settings id cannot be empty when committing device settings".to_owned(),
            ));
        }
        let settings = settings.normalized();
        let pending = self
            .pending
            .lock()
            .map_err(pending_lock_error)?
            .take()
            .ok_or_else(|| {
                DogiError::InvalidArgument(
                    "settings must be prepared and reviewed before commit".to_owned(),
                )
            })?;
        if pending.device_id != device_id || pending.settings != settings || pending.plan != *plan {
            return Err(DogiError::InvalidArgument(
                "settings changed after the apply preview was prepared".to_owned(),
            ));
        }

        let transaction_path = self.transaction_path_for(&pending.transaction)?;
        let _lock = self.acquire_transaction_lock(&transaction_path)?;
        self.ensure_interrupted_transaction_recovered_locked(&transaction_path)?;

        let mut journal = StoredDeviceTransaction {
            version: DEVICE_TRANSACTION_FILE_VERSION,
            phase: DeviceTransactionPhase::Prepared,
            transaction: pending.transaction,
            settings_id: Some(settings_id.to_owned()),
            settings: Some(settings.clone()),
        };
        self.write_device_transaction(&transaction_path, &journal)?;
        let report = match crate::hid::execute_prepared_master3s_settings_transaction(
            &journal.transaction,
        ) {
            Ok(report) => report,
            Err(error) => {
                self.clear_device_transaction(&transaction_path)?;
                return Err(error);
            }
        };
        if !report.committed() {
            if report.transaction != SettingsTransactionState::RecoveryRequired {
                self.clear_device_transaction(&transaction_path)?;
            }
            return Ok((report, self.master3s_settings_path()?));
        }

        journal.phase = DeviceTransactionPhase::DeviceCommitted;
        self.write_device_transaction(&transaction_path, &journal)?;
        match self.save_master3s_settings_for_device(settings_id, &settings) {
            Ok(path) => {
                self.clear_device_transaction(&transaction_path)?;
                Ok((report, path))
            }
            Err(save_error) => {
                journal.phase = DeviceTransactionPhase::Prepared;
                self.write_device_transaction(&transaction_path, &journal)?;
                let recovery = crate::hid::recover_prepared_master3s_settings_transaction(
                    &journal.transaction,
                )?;
                if recovery.transaction != SettingsTransactionState::RecoveryRequired {
                    self.clear_device_transaction(&transaction_path)?;
                }
                Err(DogiError::Config(format!(
                    "settings were not saved; device rollback was {}: {save_error}",
                    if recovery.transaction == SettingsTransactionState::RolledBack {
                        "verified"
                    } else {
                        "incomplete"
                    }
                )))
            }
        }
    }

    pub fn recover_interrupted_settings_transaction(&self) -> Result<Option<SettingsApplyReport>> {
        self.recover_all_interrupted_transactions(RecoveryMode::Gui)
    }

    pub fn recover_interrupted_runtime_transaction(&self) -> Result<Option<SettingsApplyReport>> {
        self.recover_all_interrupted_transactions(RecoveryMode::Runtime)
    }

    pub fn listen_master3s_runtime_events(
        &self,
        device_id: &str,
        event_limit: usize,
        idle_timeout: Duration,
    ) -> Result<Vec<Master3sRuntimeEvent>> {
        crate::hid::listen_master3s_runtime_events(device_id, event_limit, idle_timeout)
    }

    pub fn open_master3s_runtime_event_listener(
        &self,
        device_id: &str,
    ) -> Result<Master3sRuntimeEventListener> {
        Master3sRuntimeEventListener::open(device_id)
    }

    pub fn execute_master3s_runtime_actions(
        &self,
        actions: &[ResolvedRuntimeAction],
    ) -> Result<Vec<RuntimeActionExecution>> {
        runtime::execute_runtime_actions(actions)
    }

    pub fn active_application(&self) -> Result<Option<ActiveApplication>> {
        focus::active_application()
    }

    pub fn master3s_settings_path(&self) -> Result<PathBuf> {
        self.config_path.clone().ok_or_else(|| {
            DogiError::Config(
                "device settings are unavailable without an application environment".to_owned(),
            )
        })
    }

    pub fn load_master3s_settings(&self) -> Result<Master3sSettings> {
        let path = self.master3s_settings_path()?;
        let _lock = self.acquire_settings_lock(&path)?;
        Ok(self.load_master3s_settings_store_locked(&path)?.default)
    }

    pub fn load_master3s_settings_for_device(&self, device_id: &str) -> Result<Master3sSettings> {
        let path = self.master3s_settings_path()?;
        let _lock = self.acquire_settings_lock(&path)?;
        let store = self.load_master3s_settings_store_locked(&path)?;
        Ok(store
            .devices
            .get(device_id)
            .cloned()
            .unwrap_or(store.default))
    }

    pub fn save_master3s_settings(&self, settings: &Master3sSettings) -> Result<PathBuf> {
        let path = self.master3s_settings_path()?;
        let _lock = self.acquire_settings_lock(&path)?;
        let mut store = self.load_master3s_settings_store_locked(&path)?;
        store.default = settings.normalized();
        write_settings_file(&path, &store.normalized(), self.config_owner)?;
        Ok(path)
    }

    pub fn save_master3s_settings_for_device(
        &self,
        device_id: &str,
        settings: &Master3sSettings,
    ) -> Result<PathBuf> {
        let device_id = device_id.trim();
        if device_id.is_empty() {
            return Err(DogiError::InvalidArgument(
                "device id cannot be empty when saving device settings".to_owned(),
            ));
        }

        let path = self.master3s_settings_path()?;
        let _lock = self.acquire_settings_lock(&path)?;
        let mut store = self.load_master3s_settings_store_locked(&path)?;
        store
            .devices
            .insert(device_id.to_owned(), settings.normalized());
        write_settings_file(&path, &store.normalized(), self.config_owner)?;
        Ok(path)
    }

    pub fn reset_master3s_settings(&self) -> Result<PathBuf> {
        self.save_master3s_settings(&Master3sSettings::default())
    }

    fn acquire_settings_lock(&self, path: &Path) -> Result<ExclusiveFileLock> {
        ExclusiveFileLock::acquire(&sibling_lock_path(path), self.config_owner)
            .map_err(persistence_error)
    }

    fn load_master3s_settings_store_locked(&self, path: &Path) -> Result<StoredMaster3sSettings> {
        let contents = match fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(StoredMaster3sSettings::default());
            }
            Err(error) => {
                return Err(DogiError::Config(format!(
                    "failed to read {}: {error}",
                    path.display()
                )));
            }
        };

        let value = match serde_json::from_str::<serde_json::Value>(&contents) {
            Ok(value) => value,
            Err(error) => {
                return self.recover_invalid_settings_file(
                    path,
                    "invalid-json",
                    format!("invalid JSON: {error}"),
                );
            }
        };
        let version = value
            .get("version")
            .and_then(serde_json::Value::as_u64)
            .and_then(|version| u8::try_from(version).ok());
        match version {
            Some(SETTINGS_FILE_VERSION) => {
                match serde_json::from_value::<StoredMaster3sSettings>(value) {
                    Ok(settings) => settings.validated(),
                    Err(error) => self.recover_invalid_settings_file(
                        path,
                        "invalid-schema",
                        format!("schema {SETTINGS_FILE_VERSION} is malformed: {error}"),
                    ),
                }
            }
            Some(version) => self.recover_invalid_settings_file(
                path,
                "unsupported-schema",
                format!("unsupported schema version {version}"),
            ),
            None => self.recover_invalid_settings_file(
                path,
                "missing-schema",
                "the schema version is missing or invalid".to_owned(),
            ),
        }
    }

    fn recover_invalid_settings_file(
        &self,
        path: &Path,
        label: &str,
        reason: String,
    ) -> Result<StoredMaster3sSettings> {
        let backup = quarantine(path, label).map_err(persistence_error)?;
        let defaults = StoredMaster3sSettings::default();
        write_settings_file(path, &defaults, self.config_owner)?;
        let backup_detail = backup
            .as_deref()
            .map(|backup| format!(" The original was preserved at {}.", backup.display()))
            .unwrap_or_default();
        self.record_recovery_notice(format!(
            "Device settings were restored to defaults because {reason}.{backup_detail}"
        ))?;
        Ok(defaults)
    }

    fn transaction_path_for(&self, transaction: &PreparedSettingsTransaction) -> Result<PathBuf> {
        let Some(directory) = self.transaction_dir.as_deref() else {
            return Err(DogiError::Config(
                "device transaction storage is unavailable without an application environment"
                    .to_owned(),
            ));
        };
        let key = transaction
            .stable_device_key()
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() || character == '-' {
                    character
                } else {
                    '_'
                }
            })
            .collect::<String>();
        Ok(directory.join(format!("{key}.json")))
    }

    fn acquire_transaction_lock(&self, path: &Path) -> Result<ExclusiveFileLock> {
        ExclusiveFileLock::acquire(&sibling_lock_path(path), self.config_owner)
            .map_err(persistence_error)
    }

    fn ensure_interrupted_transaction_recovered_locked(&self, path: &Path) -> Result<()> {
        if self
            .recover_interrupted_transaction_locked(path, RecoveryMode::Gui)?
            .is_some_and(|report| report.transaction == SettingsTransactionState::RecoveryRequired)
        {
            return Err(DogiError::Config(format!(
                "the previous settings transaction for this device still needs recovery ({})",
                path.display()
            )));
        }
        Ok(())
    }

    fn recover_all_interrupted_transactions(
        &self,
        mode: RecoveryMode,
    ) -> Result<Option<SettingsApplyReport>> {
        self.quarantine_legacy_transaction()?;
        let Some(directory) = self.transaction_dir.as_deref() else {
            return Ok(None);
        };
        let entries = match fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(DogiError::Config(format!(
                    "failed to enumerate {}: {error}",
                    directory.display()
                )));
            }
        };

        let mut paths = entries
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| {
                path.extension().and_then(|extension| extension.to_str()) == Some("json")
            })
            .collect::<Vec<_>>();
        paths.sort();

        let mut first_report = None;
        let mut failures = Vec::new();
        for path in paths {
            let result = (|| {
                let _lock = self.acquire_transaction_lock(&path)?;
                self.recover_interrupted_transaction_locked(&path, mode)
            })();
            match result {
                Ok(Some(report)) => {
                    if first_report.is_none() {
                        first_report = Some(report);
                    }
                }
                Ok(None) => {}
                Err(error) => failures.push(format!("{}: {error}", path.display())),
            }
        }

        if failures.is_empty() {
            Ok(first_report)
        } else {
            Err(DogiError::Config(format!(
                "some device transactions could not be recovered; other devices were still processed: {}",
                failures.join("; ")
            )))
        }
    }

    fn recover_interrupted_transaction_locked(
        &self,
        path: &Path,
        mode: RecoveryMode,
    ) -> Result<Option<SettingsApplyReport>> {
        let Some(journal) = self.load_device_transaction(path)? else {
            return Ok(None);
        };
        match (journal.phase, mode) {
            (DeviceTransactionPhase::Prepared, _)
            | (DeviceTransactionPhase::DeviceCommitted, RecoveryMode::Runtime) => {
                let report = crate::hid::recover_prepared_master3s_settings_transaction(
                    &journal.transaction,
                )?;
                if report.transaction != SettingsTransactionState::RecoveryRequired {
                    self.clear_device_transaction(path)?;
                }
                Ok(Some(report))
            }
            (DeviceTransactionPhase::DeviceCommitted, RecoveryMode::Gui) => {
                let settings_id = journal.settings_id.as_deref().ok_or_else(|| {
                    DogiError::Config(
                        "committed device transaction has no settings identifier".to_owned(),
                    )
                })?;
                let settings = journal.settings.as_ref().ok_or_else(|| {
                    DogiError::Config(
                        "committed device transaction has no settings payload".to_owned(),
                    )
                })?;
                self.save_master3s_settings_for_device(settings_id, settings)?;
                self.clear_device_transaction(path)?;
                Ok(None)
            }
        }
    }

    fn load_device_transaction(&self, path: &Path) -> Result<Option<StoredDeviceTransaction>> {
        let contents = match fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(DogiError::Config(format!(
                    "failed to read {}: {error}",
                    path.display()
                )));
            }
        };
        let journal = serde_json::from_str::<StoredDeviceTransaction>(&contents);
        let Ok(journal) = journal else {
            let backup = quarantine(path, "invalid-transaction").map_err(persistence_error)?;
            self.record_recovery_notice(format!(
                "An invalid device transaction was preserved at {} and ignored.",
                backup.as_deref().unwrap_or(path).display()
            ))?;
            return Ok(None);
        };
        if journal.version != DEVICE_TRANSACTION_FILE_VERSION {
            let backup = quarantine(path, "unsupported-transaction").map_err(persistence_error)?;
            self.record_recovery_notice(format!(
                "An unsupported device transaction was preserved at {} and ignored.",
                backup.as_deref().unwrap_or(path).display()
            ))?;
            return Ok(None);
        }
        Ok(Some(journal))
    }

    fn write_device_transaction(
        &self,
        path: &Path,
        transaction: &StoredDeviceTransaction,
    ) -> Result<()> {
        write_json_file(path, transaction, self.config_owner)
    }

    fn clear_device_transaction(&self, path: &Path) -> Result<()> {
        durable_remove(path).map_err(persistence_error)
    }

    fn quarantine_legacy_transaction(&self) -> Result<()> {
        let Some(path) = self.legacy_transaction_path.as_deref() else {
            return Ok(());
        };
        if let Some(backup) = quarantine(path, "legacy-transaction").map_err(persistence_error)? {
            self.record_recovery_notice(format!(
                "A legacy device transaction without a safe device fingerprint was preserved at {} and was not replayed.",
                backup.display()
            ))?;
        }
        Ok(())
    }

    fn record_recovery_notice(&self, notice: String) -> Result<()> {
        *self.recovery_notice.lock().map_err(pending_lock_error)? = Some(notice);
        Ok(())
    }

    pub(crate) fn take_recovery_notice(&self) -> Option<String> {
        self.recovery_notice.lock().ok()?.take()
    }
}

fn pending_lock_error<T>(_: std::sync::PoisonError<T>) -> DogiError {
    DogiError::Config("device settings transaction state is unavailable".to_owned())
}

fn persistence_error(error: crate::persistence::PersistenceError) -> DogiError {
    DogiError::Config(error.to_string())
}

fn write_settings_file(
    path: &Path,
    settings: &StoredMaster3sSettings,
    owner: Option<FileOwner>,
) -> Result<()> {
    write_json_file(path, settings, owner)
}

fn write_json_file<T: Serialize>(path: &Path, value: &T, owner: Option<FileOwner>) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| DogiError::Config(format!("failed to serialize settings: {error}")))?;
    bytes.push(b'\n');
    atomic_write(path, &bytes, owner).map_err(persistence_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saves_and_loads_settings_from_config_path() {
        let fixture = unique_test_config("roundtrip");
        let path = fixture.path.clone();
        let daemon = DeviceService::with_config_path(&path);
        let settings = Master3sSettings {
            pointer_speed_percent: 125,
            ..Master3sSettings::default()
        };

        let saved_path = daemon.save_master3s_settings(&settings).unwrap();
        let loaded = daemon.load_master3s_settings().unwrap();

        assert_eq!(saved_path, path);
        assert_eq!(loaded.pointer_speed_percent, 125);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn missing_settings_file_uses_default() {
        let fixture = unique_test_config("missing");
        let path = fixture.path.clone();
        let daemon = DeviceService::with_config_path(path);

        assert_eq!(
            daemon.load_master3s_settings().unwrap(),
            Master3sSettings::default()
        );
    }

    #[test]
    fn saved_settings_are_normalized() {
        let fixture = unique_test_config("normalized");
        let path = fixture.path.clone();
        let daemon = DeviceService::with_config_path(&path);
        let settings = Master3sSettings {
            pointer_speed_percent: 250,
            ..Master3sSettings::default()
        };

        daemon.save_master3s_settings(&settings).unwrap();
        let loaded = daemon.load_master3s_settings().unwrap();

        assert_eq!(loaded.pointer_speed_percent, 200);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn unversioned_settings_are_preserved_and_replaced_with_defaults() {
        let fixture = unique_test_config("legacy-recovered");
        let path = fixture.path.clone();
        let daemon = DeviceService::with_config_path(&path);
        let legacy = Master3sSettings {
            pointer_speed_percent: 135,
            ..Master3sSettings::default()
        };
        fs::write(&path, serde_json::to_vec_pretty(&legacy).unwrap()).unwrap();

        let loaded = daemon.load_master3s_settings().unwrap();
        assert_eq!(loaded, Master3sSettings::default());
        assert!(
            daemon
                .take_recovery_notice()
                .is_some_and(|notice| notice.contains("preserved"))
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn obsolete_settings_schema_is_preserved_and_reset() {
        let fixture = unique_test_config("schema-four");
        let path = fixture.path.clone();
        let daemon = DeviceService::with_config_path(&path);
        let store = StoredMaster3sSettings {
            version: 4,
            default: Master3sSettings {
                pointer_speed_percent: 135,
                ..Master3sSettings::default()
            },
            ..StoredMaster3sSettings::default()
        };
        fs::write(&path, serde_json::to_vec_pretty(&store).unwrap()).unwrap();

        assert_eq!(
            daemon.load_master3s_settings().unwrap(),
            Master3sSettings::default()
        );
        assert!(
            daemon
                .take_recovery_notice()
                .is_some_and(|notice| notice.contains("unsupported schema version 4"))
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn future_settings_schema_is_preserved_and_reset() {
        let fixture = unique_test_config("future-version");
        let path = fixture.path.clone();
        let daemon = DeviceService::with_config_path(&path);
        let store = StoredMaster3sSettings {
            version: SETTINGS_FILE_VERSION + 1,
            ..StoredMaster3sSettings::default()
        };
        fs::write(&path, serde_json::to_vec_pretty(&store).unwrap()).unwrap();

        assert_eq!(
            daemon.load_master3s_settings().unwrap(),
            Master3sSettings::default()
        );
        assert!(
            daemon
                .take_recovery_notice()
                .is_some_and(|notice| notice.contains("unsupported schema version"))
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn device_settings_are_isolated_and_fall_back_to_default() {
        let fixture = unique_test_config("per-device");
        let path = fixture.path.clone();
        let daemon = DeviceService::with_config_path(&path);
        let default = Master3sSettings {
            pointer_speed_percent: 90,
            ..Master3sSettings::default()
        };
        let device_a = Master3sSettings {
            pointer_speed_percent: 125,
            ..Master3sSettings::default()
        };

        daemon.save_master3s_settings(&default).unwrap();
        daemon
            .save_master3s_settings_for_device("device-a", &device_a)
            .unwrap();

        assert_eq!(
            daemon
                .load_master3s_settings_for_device("device-a")
                .unwrap()
                .pointer_speed_percent,
            125
        );
        assert_eq!(
            daemon
                .load_master3s_settings_for_device("device-b")
                .unwrap()
                .pointer_speed_percent,
            90
        );
        assert_eq!(
            daemon
                .load_master3s_settings()
                .unwrap()
                .pointer_speed_percent,
            90
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn updating_default_preserves_saved_device_settings() {
        let fixture = unique_test_config("preserve-device");
        let path = fixture.path.clone();
        let daemon = DeviceService::with_config_path(&path);
        let device = Master3sSettings {
            pointer_speed_percent: 140,
            ..Master3sSettings::default()
        };

        daemon
            .save_master3s_settings_for_device("device-a", &device)
            .unwrap();
        daemon
            .save_master3s_settings(&Master3sSettings {
                pointer_speed_percent: 80,
                ..Master3sSettings::default()
            })
            .unwrap();

        assert_eq!(
            daemon
                .load_master3s_settings_for_device("device-a")
                .unwrap()
                .pointer_speed_percent,
            140
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn concurrent_device_updates_do_not_lose_each_other() {
        let fixture = unique_test_config("concurrent-rmw");
        let path = fixture.path.clone();
        let daemon = DeviceService::with_config_path(&path);
        let first = daemon.clone();
        let second = daemon.clone();
        let first_thread = std::thread::spawn(move || {
            first
                .save_master3s_settings_for_device(
                    "device-a",
                    &Master3sSettings {
                        pointer_speed_percent: 110,
                        ..Master3sSettings::default()
                    },
                )
                .unwrap();
        });
        let second_thread = std::thread::spawn(move || {
            second
                .save_master3s_settings_for_device(
                    "device-b",
                    &Master3sSettings {
                        pointer_speed_percent: 140,
                        ..Master3sSettings::default()
                    },
                )
                .unwrap();
        });
        first_thread.join().unwrap();
        second_thread.join().unwrap();

        assert_eq!(
            daemon
                .load_master3s_settings_for_device("device-a")
                .unwrap()
                .pointer_speed_percent,
            110
        );
        assert_eq!(
            daemon
                .load_master3s_settings_for_device("device-b")
                .unwrap()
                .pointer_speed_percent,
            140
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn committed_device_transaction_rolls_configuration_forward_after_restart() {
        let fixture = unique_test_config("transaction-roll-forward");
        let path = fixture.path.clone();
        let daemon = DeviceService::with_config_path(&path);
        let transaction: PreparedSettingsTransaction = serde_json::from_value(serde_json::json!({
            "version": 2,
            "device_id": "receiver:slot:01:wpid:B034",
            "identity": {
                "receiver_id": "receiver",
                "receiver_vendor_id": 1133,
                "receiver_product_id": 50504,
                "receiver_serial": "receiver-a",
                "slot": 1,
                "wpid": "B034",
                "unit_id": "AABBCCDD",
                "model_id": "B03400000000"
            },
            "profile_name": "Default",
            "changes": []
        }))
        .unwrap();
        let transaction_path = daemon.transaction_path_for(&transaction).unwrap();
        let settings = Master3sSettings {
            pointer_speed_percent: 135,
            ..Master3sSettings::default()
        };
        daemon
            .write_device_transaction(
                &transaction_path,
                &StoredDeviceTransaction {
                    version: DEVICE_TRANSACTION_FILE_VERSION,
                    phase: DeviceTransactionPhase::DeviceCommitted,
                    transaction,
                    settings_id: Some("device-a".to_owned()),
                    settings: Some(settings),
                },
            )
            .unwrap();

        assert!(
            daemon
                .recover_interrupted_settings_transaction()
                .unwrap()
                .is_none()
        );
        assert_eq!(
            daemon
                .load_master3s_settings_for_device("device-a")
                .unwrap()
                .pointer_speed_percent,
            135
        );
        assert!(
            daemon
                .load_device_transaction(&transaction_path)
                .unwrap()
                .is_none()
        );

        let _ = fs::remove_file(&path);
        if let Some(path) = daemon.transaction_dir {
            let _ = fs::remove_dir_all(path);
        }
    }

    #[test]
    fn a_corrupt_device_journal_does_not_block_another_device() {
        let fixture = unique_test_config("journal-isolation");
        let path = fixture.path.clone();
        let daemon = DeviceService::with_config_path(&path);
        let transaction = test_transaction(2, "EEFF0011");
        let transaction_path = daemon.transaction_path_for(&transaction).unwrap();
        daemon
            .write_device_transaction(
                &transaction_path,
                &StoredDeviceTransaction {
                    version: DEVICE_TRANSACTION_FILE_VERSION,
                    phase: DeviceTransactionPhase::DeviceCommitted,
                    transaction,
                    settings_id: Some("device-b".to_owned()),
                    settings: Some(Master3sSettings {
                        pointer_speed_percent: 145,
                        ..Master3sSettings::default()
                    }),
                },
            )
            .unwrap();
        let corrupt_path = daemon
            .transaction_dir
            .as_ref()
            .unwrap()
            .join("aaa-corrupt.json");
        fs::write(&corrupt_path, b"not-json").unwrap();

        assert!(
            daemon
                .recover_interrupted_settings_transaction()
                .unwrap()
                .is_none()
        );
        assert_eq!(
            daemon
                .load_master3s_settings_for_device("device-b")
                .unwrap()
                .pointer_speed_percent,
            145
        );
        assert!(!transaction_path.exists());
        assert!(!corrupt_path.exists());

        let _ = fs::remove_file(path);
        if let Some(path) = daemon.transaction_dir {
            let _ = fs::remove_dir_all(path);
        }
    }

    #[test]
    fn plans_local_runtime_actions() {
        let daemon = DeviceService::new();
        let settings = Master3sSettings {
            thumb_wheel: crate::domain::ThumbWheelMode::Zoom,
            ..Master3sSettings::default()
        };

        let plan = daemon.plan_master3s_runtime(&settings);

        assert!(plan.requires_listener());
        assert!(plan.summary().contains("thumb wheel zoom"));
    }

    struct TestConfig {
        root: PathBuf,
        path: PathBuf,
    }

    impl Drop for TestConfig {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn unique_test_config(name: &str) -> TestConfig {
        use std::sync::atomic::{AtomicU64, Ordering};

        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "dogi-device-{name}-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let path = root.join("master3s.json");
        TestConfig { root, path }
    }

    fn test_transaction(slot: u8, unit_id: &str) -> PreparedSettingsTransaction {
        serde_json::from_value(serde_json::json!({
            "version": 2,
            "device_id": format!("receiver:slot:{slot:02x}:wpid:B034"),
            "identity": {
                "receiver_id": "receiver",
                "receiver_vendor_id": 1133,
                "receiver_product_id": 50504,
                "receiver_serial": "receiver-a",
                "slot": slot,
                "wpid": "B034",
                "unit_id": unit_id,
                "model_id": "B03400000000"
            },
            "profile_name": "Default",
            "changes": []
        }))
        .unwrap()
    }
}

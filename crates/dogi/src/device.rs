use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[cfg(unix)]
use std::os::fd::AsRawFd;

use dogi_core::{
    ActiveApplication, DeviceInfo, DogiError, LocalRuntimePlan, Master3sSettings,
    ResolvedRuntimeAction, Result, SettingsApplyPlan, SettingsApplyPreview, SettingsApplyReport,
    SettingsTransactionState,
};
use serde::{Deserialize, Serialize};

use dogi_core::Master3sRuntimeEvent;
use dogi_hid::{Master3sRuntimeEventListener, PreparedSettingsTransaction};

use crate::desktop::focus;
use crate::environment::AppEnvironment;
use crate::runtime::lock::ProcessLock;
use crate::runtime::{self, RuntimeActionExecution};

#[derive(Clone, Debug, Default)]
pub(crate) struct DeviceService {
    config_path: Option<PathBuf>,
    config_owner: Option<ConfigFileOwner>,
    transaction_path: Option<PathBuf>,
    transaction_lock_path: Option<PathBuf>,
    pending: Arc<Mutex<Option<PendingSettingsTransaction>>>,
}

#[derive(Clone, Debug)]
struct PendingSettingsTransaction {
    device_id: String,
    settings: Master3sSettings,
    plan: SettingsApplyPlan,
    transaction: PreparedSettingsTransaction,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ConfigFileOwner {
    uid: u32,
    gid: u32,
}

impl ConfigFileOwner {
    pub(crate) fn new(uid: u32, gid: u32) -> Self {
        Self { uid, gid }
    }
}

const SETTINGS_FILE_VERSION: u8 = 5;
const DEVICE_TRANSACTION_FILE_VERSION: u8 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DeviceTransactionPhase {
    Prepared,
    DeviceCommitted,
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
            .map(|(uid, gid)| ConfigFileOwner::new(uid, gid));
        Self {
            config_path: Some(environment.paths.device_settings()),
            config_owner: owner,
            transaction_path: Some(environment.paths.device_transaction()),
            transaction_lock_path: Some(environment.paths.device_transaction_lock()),
            pending: Arc::default(),
        }
    }

    #[cfg(test)]
    pub fn with_config_path(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let transaction_path = path.with_extension("transaction.json");
        let transaction_lock_path = path.with_extension("transaction.lock");
        Self {
            config_path: Some(path),
            config_owner: None,
            transaction_path: Some(transaction_path),
            transaction_lock_path: Some(transaction_lock_path),
            pending: Arc::default(),
        }
    }

    pub fn scan_devices(&self) -> Result<Vec<DeviceInfo>> {
        dogi_hid::scan_devices()
    }

    pub fn scan_device_inventory(&self) -> Result<Vec<DeviceInfo>> {
        dogi_hid::scan_device_inventory()
    }

    pub fn scan_devices_for_ui(&self) -> Result<Vec<DeviceInfo>> {
        dogi_hid::scan_devices_for_ui()
    }

    pub fn scan_all_devices(&self) -> Result<Vec<DeviceInfo>> {
        dogi_hid::scan_all_devices()
    }

    pub fn find_device(&self, id: &str) -> Result<DeviceInfo> {
        dogi_hid::find_device(id)
    }

    pub fn plan_master3s_settings(
        &self,
        device_id: &str,
        settings: &Master3sSettings,
    ) -> SettingsApplyPlan {
        dogi_core::build_master3s_apply_plan(device_id, &settings.normalized())
    }

    pub fn plan_master3s_runtime(&self, settings: &Master3sSettings) -> LocalRuntimePlan {
        dogi_core::build_master3s_runtime_plan(&settings.normalized())
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
        let _lock = self.acquire_transaction_lock()?;
        self.ensure_interrupted_transaction_recovered_locked()?;
        let transaction =
            dogi_hid::prepare_master3s_settings_plan(device_id, &settings.normalized(), plan)?;
        let journal = StoredDeviceTransaction {
            version: DEVICE_TRANSACTION_FILE_VERSION,
            phase: DeviceTransactionPhase::Prepared,
            transaction: transaction.clone(),
            settings_id: None,
            settings: None,
        };
        self.write_device_transaction(&journal)?;
        let report = match dogi_hid::execute_prepared_master3s_settings_transaction(&transaction) {
            Ok(report) => report,
            Err(error) => {
                self.clear_device_transaction()?;
                return Err(error);
            }
        };
        if report.transaction != SettingsTransactionState::RecoveryRequired {
            self.clear_device_transaction()?;
        }
        Ok(report)
    }

    pub fn prepare_master3s_settings_transaction(
        &self,
        device_id: &str,
        settings: &Master3sSettings,
        plan: &SettingsApplyPlan,
    ) -> Result<SettingsApplyPreview> {
        let _lock = self.acquire_transaction_lock()?;
        self.ensure_interrupted_transaction_recovered_locked()?;
        let settings = settings.normalized();
        let transaction = dogi_hid::prepare_master3s_settings_plan(device_id, &settings, plan)?;
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
        let _lock = self.acquire_transaction_lock()?;
        self.ensure_interrupted_transaction_recovered_locked()?;
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

        let mut journal = StoredDeviceTransaction {
            version: DEVICE_TRANSACTION_FILE_VERSION,
            phase: DeviceTransactionPhase::Prepared,
            transaction: pending.transaction,
            settings_id: Some(settings_id.to_owned()),
            settings: Some(settings.clone()),
        };
        self.write_device_transaction(&journal)?;
        let report =
            match dogi_hid::execute_prepared_master3s_settings_transaction(&journal.transaction) {
                Ok(report) => report,
                Err(error) => {
                    self.clear_device_transaction()?;
                    return Err(error);
                }
            };
        if !report.committed() {
            if report.transaction != SettingsTransactionState::RecoveryRequired {
                self.clear_device_transaction()?;
            }
            return Ok((report, self.master3s_settings_path()?));
        }

        journal.phase = DeviceTransactionPhase::DeviceCommitted;
        self.write_device_transaction(&journal)?;
        match self.save_master3s_settings_for_device(settings_id, &settings) {
            Ok(path) => {
                self.clear_device_transaction()?;
                Ok((report, path))
            }
            Err(save_error) => {
                journal.phase = DeviceTransactionPhase::Prepared;
                self.write_device_transaction(&journal)?;
                let recovery =
                    dogi_hid::recover_prepared_master3s_settings_transaction(&journal.transaction)?;
                if recovery.transaction != SettingsTransactionState::RecoveryRequired {
                    self.clear_device_transaction()?;
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
        let _lock = self.acquire_transaction_lock()?;
        self.recover_interrupted_transaction_locked()
    }

    pub fn listen_master3s_runtime_events(
        &self,
        device_id: &str,
        event_limit: usize,
        idle_timeout: Duration,
    ) -> Result<Vec<Master3sRuntimeEvent>> {
        dogi_hid::listen_master3s_runtime_events(device_id, event_limit, idle_timeout)
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
        Ok(self.load_master3s_settings_store()?.default)
    }

    pub fn load_master3s_settings_for_device(&self, device_id: &str) -> Result<Master3sSettings> {
        let store = self.load_master3s_settings_store()?;
        Ok(store
            .devices
            .get(device_id)
            .cloned()
            .unwrap_or(store.default))
    }

    pub fn save_master3s_settings(&self, settings: &Master3sSettings) -> Result<PathBuf> {
        let path = self.master3s_settings_path()?;
        let mut store = self.load_master3s_settings_store()?;
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
        let mut store = self.load_master3s_settings_store()?;
        store
            .devices
            .insert(device_id.to_owned(), settings.normalized());
        write_settings_file(&path, &store.normalized(), self.config_owner)?;
        Ok(path)
    }

    pub fn reset_master3s_settings(&self) -> Result<PathBuf> {
        self.save_master3s_settings(&Master3sSettings::default())
    }

    fn load_master3s_settings_store(&self) -> Result<StoredMaster3sSettings> {
        let path = self.master3s_settings_path()?;
        let contents = match fs::read_to_string(&path) {
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

        let persisted =
            serde_json::from_str::<StoredMaster3sSettings>(&contents).map_err(|error| {
                DogiError::Config(format!("failed to parse {}: {error}", path.display()))
            })?;
        persisted.validated()
    }

    fn acquire_transaction_lock(&self) -> Result<Option<ProcessLock>> {
        self.transaction_lock_path
            .as_deref()
            .map(|path| ProcessLock::acquire(path, "device settings transaction"))
            .transpose()
    }

    fn ensure_interrupted_transaction_recovered_locked(&self) -> Result<()> {
        if self
            .recover_interrupted_transaction_locked()?
            .is_some_and(|report| report.transaction == SettingsTransactionState::RecoveryRequired)
        {
            return Err(DogiError::Config(
                "the previous device settings transaction still needs recovery".to_owned(),
            ));
        }
        Ok(())
    }

    fn recover_interrupted_transaction_locked(&self) -> Result<Option<SettingsApplyReport>> {
        let Some(journal) = self.load_device_transaction()? else {
            return Ok(None);
        };
        match journal.phase {
            DeviceTransactionPhase::Prepared => {
                let report =
                    dogi_hid::recover_prepared_master3s_settings_transaction(&journal.transaction)?;
                if report.transaction != SettingsTransactionState::RecoveryRequired {
                    self.clear_device_transaction()?;
                }
                Ok(Some(report))
            }
            DeviceTransactionPhase::DeviceCommitted => {
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
                self.clear_device_transaction()?;
                Ok(None)
            }
        }
    }

    fn load_device_transaction(&self) -> Result<Option<StoredDeviceTransaction>> {
        let Some(path) = self.transaction_path.as_deref() else {
            return Ok(None);
        };
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
        let journal: StoredDeviceTransaction =
            serde_json::from_str(&contents).map_err(|error| {
                DogiError::Config(format!("failed to parse {}: {error}", path.display()))
            })?;
        if journal.version != DEVICE_TRANSACTION_FILE_VERSION {
            return Err(DogiError::Config(format!(
                "unsupported device transaction schema version {}",
                journal.version
            )));
        }
        Ok(Some(journal))
    }

    fn write_device_transaction(&self, transaction: &StoredDeviceTransaction) -> Result<()> {
        let Some(path) = self.transaction_path.as_deref() else {
            return Ok(());
        };
        write_json_file(path, transaction, self.config_owner)
    }

    fn clear_device_transaction(&self) -> Result<()> {
        let Some(path) = self.transaction_path.as_deref() else {
            return Ok(());
        };
        match fs::remove_file(path) {
            Ok(()) => {
                if let Some(parent) = path.parent() {
                    sync_directory(parent);
                }
                Ok(())
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(DogiError::Config(format!(
                "failed to clear {}: {error}",
                path.display()
            ))),
        }
    }
}

fn pending_lock_error<T>(_: std::sync::PoisonError<T>) -> DogiError {
    DogiError::Config("device settings transaction state is unavailable".to_owned())
}

fn write_settings_file(
    path: &Path,
    settings: &StoredMaster3sSettings,
    owner: Option<ConfigFileOwner>,
) -> Result<()> {
    write_json_file(path, settings, owner)
}

fn write_json_file<T: Serialize>(
    path: &Path,
    value: &T,
    owner: Option<ConfigFileOwner>,
) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        DogiError::Config(format!("settings path has no parent: {}", path.display()))
    })?;
    fs::create_dir_all(parent).map_err(|error| {
        DogiError::Config(format!("failed to create {}: {error}", parent.display()))
    })?;
    set_path_owner(parent, owner)?;

    let tmp_path = path.with_extension(format!("json.tmp-{}", std::process::id()));
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&tmp_path)
        .map_err(|error| {
            DogiError::Config(format!("failed to write {}: {error}", tmp_path.display()))
        })?;
    set_file_owner(&file, owner, &tmp_path)?;
    let mut writer = BufWriter::new(file);
    serde_json::to_writer_pretty(&mut writer, value)
        .map_err(|error| DogiError::Config(format!("failed to serialize settings: {error}")))?;
    writer.write_all(b"\n").map_err(|error| {
        DogiError::Config(format!("failed to write {}: {error}", tmp_path.display()))
    })?;
    writer.flush().map_err(|error| {
        DogiError::Config(format!("failed to flush {}: {error}", tmp_path.display()))
    })?;
    writer
        .into_inner()
        .map_err(|error| {
            DogiError::Config(format!(
                "failed to flush {}: {}",
                tmp_path.display(),
                error.into_error()
            ))
        })?
        .sync_all()
        .map_err(|error| {
            DogiError::Config(format!("failed to sync {}: {error}", tmp_path.display()))
        })?;
    fs::rename(&tmp_path, path).map_err(|error| {
        DogiError::Config(format!(
            "failed to replace {} with {}: {error}",
            path.display(),
            tmp_path.display()
        ))
    })?;
    sync_directory(parent);
    Ok(())
}

fn set_path_owner(path: &Path, owner: Option<ConfigFileOwner>) -> Result<()> {
    let file = File::open(path).map_err(|error| {
        DogiError::Config(format!(
            "failed to preserve ownership of {}: {error}",
            path.display()
        ))
    })?;
    set_file_owner(&file, owner, path)
}

#[cfg(unix)]
fn set_file_owner(file: &File, owner: Option<ConfigFileOwner>, path: &Path) -> Result<()> {
    let Some(owner) = owner else {
        return Ok(());
    };
    let result = unsafe { libc::fchown(file.as_raw_fd(), owner.uid, owner.gid) };
    if result == 0 {
        Ok(())
    } else {
        Err(DogiError::Config(format!(
            "failed to preserve ownership of {}: {}",
            path.display(),
            io::Error::last_os_error()
        )))
    }
}

#[cfg(not(unix))]
fn set_file_owner(_file: &File, _owner: Option<ConfigFileOwner>, _path: &Path) -> Result<()> {
    Ok(())
}

fn sync_directory(path: &Path) {
    if let Ok(directory) = File::open(path) {
        let _ = directory.sync_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saves_and_loads_settings_from_config_path() {
        let path = unique_test_path("roundtrip");
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
        let path = unique_test_path("missing");
        let daemon = DeviceService::with_config_path(path);

        assert_eq!(
            daemon.load_master3s_settings().unwrap(),
            Master3sSettings::default()
        );
    }

    #[test]
    fn saved_settings_are_normalized() {
        let path = unique_test_path("normalized");
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
    fn legacy_unversioned_settings_are_rejected() {
        let path = unique_test_path("legacy-rejected");
        let daemon = DeviceService::with_config_path(&path);
        let legacy = Master3sSettings {
            pointer_speed_percent: 135,
            ..Master3sSettings::default()
        };
        fs::write(&path, serde_json::to_vec_pretty(&legacy).unwrap()).unwrap();

        let error = daemon.load_master3s_settings().unwrap_err();
        assert!(error.to_string().contains("failed to parse"));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn obsolete_settings_schema_is_rejected() {
        let path = unique_test_path("obsolete-version");
        let daemon = DeviceService::with_config_path(&path);
        let store = StoredMaster3sSettings {
            version: SETTINGS_FILE_VERSION - 1,
            ..StoredMaster3sSettings::default()
        };
        fs::write(&path, serde_json::to_vec_pretty(&store).unwrap()).unwrap();

        let error = daemon.load_master3s_settings().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unsupported settings schema version")
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn future_settings_schema_is_rejected() {
        let path = unique_test_path("future-version");
        let daemon = DeviceService::with_config_path(&path);
        let store = StoredMaster3sSettings {
            version: SETTINGS_FILE_VERSION + 1,
            ..StoredMaster3sSettings::default()
        };
        fs::write(&path, serde_json::to_vec_pretty(&store).unwrap()).unwrap();

        let error = daemon.load_master3s_settings().unwrap_err();

        assert!(
            error
                .to_string()
                .contains("unsupported settings schema version")
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn device_settings_are_isolated_and_fall_back_to_default() {
        let path = unique_test_path("per-device");
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
        let path = unique_test_path("preserve-device");
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
    fn committed_device_transaction_rolls_configuration_forward_after_restart() {
        let path = unique_test_path("transaction-roll-forward");
        let daemon = DeviceService::with_config_path(&path);
        let transaction: PreparedSettingsTransaction = serde_json::from_value(serde_json::json!({
            "version": 1,
            "device_id": "device-a",
            "profile_name": "Default",
            "changes": []
        }))
        .unwrap();
        let settings = Master3sSettings {
            pointer_speed_percent: 135,
            ..Master3sSettings::default()
        };
        daemon
            .write_device_transaction(&StoredDeviceTransaction {
                version: DEVICE_TRANSACTION_FILE_VERSION,
                phase: DeviceTransactionPhase::DeviceCommitted,
                transaction,
                settings_id: Some("device-a".to_owned()),
                settings: Some(settings),
            })
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
        assert!(daemon.load_device_transaction().unwrap().is_none());

        let _ = fs::remove_file(&path);
        if let Some(path) = daemon.transaction_lock_path {
            let _ = fs::remove_file(path);
        }
    }

    #[test]
    fn plans_local_runtime_actions() {
        let daemon = DeviceService::new();
        let settings = Master3sSettings {
            thumb_wheel: dogi_core::ThumbWheelMode::Zoom,
            ..Master3sSettings::default()
        };

        let plan = daemon.plan_master3s_runtime(&settings);

        assert!(plan.requires_listener());
        assert!(plan.summary().contains("thumb wheel zoom"));
    }

    fn unique_test_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("dogi-{name}-{}.json", std::process::id()))
    }
}

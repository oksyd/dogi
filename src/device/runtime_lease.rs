use super::*;

/// The durable snapshot is separate from an in-flight GUI commit, but both use
/// the same device transaction lock. Only the runtime lock owner releases leases.
#[derive(Debug)]
pub(crate) struct RuntimeSettingsLease {
    transaction_path: PathBuf,
}

impl DeviceService {
    pub(crate) fn replace_runtime_settings_lease(
        &self,
        lease: &mut Option<RuntimeSettingsLease>,
        device_id: &str,
        settings: &Master3sSettings,
        plan: &SettingsApplyPlan,
    ) -> Result<SettingsApplyReport> {
        self.release_runtime_settings_lease(lease)?;
        let candidate = crate::hid::prepare_master3s_runtime_plan(device_id, settings, plan)?;
        let transaction_path = self.transaction_path_for(&candidate)?;
        let _lock = self.acquire_transaction_lock(&transaction_path)?;
        self.ensure_interrupted_transaction_recovered_locked(&transaction_path)?;
        self.restore_runtime_settings_locked(&transaction_path)?;
        let transaction = crate::hid::prepare_master3s_runtime_plan(device_id, settings, plan)?;
        if self.transaction_path_for(&transaction)? != transaction_path {
            return Err(DogiError::Protocol(
                "device identity changed while acquiring runtime settings".to_owned(),
            ));
        }
        write_json_file(
            &transaction_path.with_extension("lease"),
            &transaction,
            self.config_owner,
        )?;
        // Retain recovery ownership even if execution fails partway through.
        *lease = Some(RuntimeSettingsLease { transaction_path });
        crate::hid::execute_prepared_master3s_settings_transaction(&transaction)
    }

    pub(crate) fn release_runtime_settings_lease(
        &self,
        lease: &mut Option<RuntimeSettingsLease>,
    ) -> Result<()> {
        if let Some(current) = lease {
            let _lock = self.acquire_transaction_lock(&current.transaction_path)?;
            self.ensure_interrupted_transaction_recovered_locked(&current.transaction_path)?;
            self.restore_runtime_settings_locked(&current.transaction_path)?;
            *lease = None;
        }
        Ok(())
    }

    pub(super) fn relinquish_runtime_settings_locked(
        &self,
        transaction_path: &Path,
        plan: &SettingsApplyPlan,
    ) -> Result<()> {
        let lease_path = transaction_path.with_extension("lease");
        let Some(mut transaction) = self.load_runtime_lease(&lease_path)? else {
            return Ok(());
        };
        transaction.relinquish(plan);
        write_json_file(&lease_path, &transaction, self.config_owner)
    }

    fn load_runtime_lease(&self, path: &Path) -> Result<Option<PreparedSettingsTransaction>> {
        let contents = match fs::read(path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(DogiError::Config(format!(
                    "failed to read runtime lease: {error}"
                )));
            }
        };
        let transaction: PreparedSettingsTransaction =
            serde_json::from_slice(&contents).map_err(|error| {
                DogiError::Config(format!(
                    "invalid runtime lease; preserved for recovery: {error}"
                ))
            })?;
        transaction.validate_recovery()?;
        if self
            .transaction_path_for(&transaction)?
            .with_extension("lease")
            != path
        {
            return Err(DogiError::Config(
                "runtime lease identity does not match its filename".to_owned(),
            ));
        }
        Ok(Some(transaction))
    }

    fn restore_runtime_settings_locked(&self, transaction_path: &Path) -> Result<()> {
        let path = transaction_path.with_extension("lease");
        let Some(transaction) = self.load_runtime_lease(&path)? else {
            return Ok(());
        };
        if !transaction.changes.is_empty() {
            let report = crate::hid::restore_runtime_settings(&transaction)?;
            if report.transaction != SettingsTransactionState::RolledBack {
                return Err(DogiError::BackendUnavailable(
                    "temporary mouse settings could not be fully restored; recovery snapshot retained".to_owned(),
                ));
            }
        }
        durable_remove(&path).map_err(persistence_error)
    }

    pub(super) fn recover_runtime_leases(&self) -> Result<()> {
        let Some(directory) = &self.transaction_dir else {
            return Ok(());
        };
        let entries = match fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(DogiError::Config(format!(
                    "failed to list runtime leases: {error}"
                )));
            }
        };
        for entry in entries {
            let path = entry
                .map_err(|error| DogiError::Config(error.to_string()))?
                .path();
            if path
                .extension()
                .is_none_or(|extension| extension != "lease")
            {
                continue;
            }
            let transaction_path = path.with_extension("json");
            let _lock = self.acquire_transaction_lock(&transaction_path)?;
            match self.restore_runtime_settings_locked(&transaction_path) {
                Ok(()) => {}
                // A detached mouse must not block another connected device.
                // The snapshot remains durable and is retried on the next session.
                Err(DogiError::DeviceNotFound) => {
                    log::debug!("Runtime recovery deferred until mouse reconnects");
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}

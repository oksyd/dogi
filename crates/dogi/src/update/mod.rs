pub(crate) mod github;
mod install;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use dogi_ui::{
    ApplicationUpdateCheckIntent, ApplicationUpdateError, ApplicationUpdateErrorKind,
    ApplicationUpdateManager, ApplicationUpdateOperation, ApplicationUpdateResult,
};
use semver::Version;

use self::github::{GitHubReleaseClient, ReleaseCandidate};
use self::install::{Installation, InstallationError, InstallationOutcome};
use crate::config::application::ApplicationConfigStore;
use crate::environment::AppEnvironment;

const AUTOMATIC_CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

struct PreparedUpdate {
    candidate: ReleaseCandidate,
    download: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum UpdateErrorKind {
    Policy,
    Network,
    Verification,
    Storage,
    Authorization,
    Installation,
    State,
    Internal,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct UpdateError {
    kind: UpdateErrorKind,
    detail: String,
}

impl UpdateError {
    pub(super) fn new(kind: UpdateErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
        }
    }

    pub(super) fn policy(detail: impl Into<String>) -> Self {
        Self::new(UpdateErrorKind::Policy, detail)
    }

    pub(super) fn network(detail: impl Into<String>) -> Self {
        Self::new(UpdateErrorKind::Network, detail)
    }

    pub(super) fn verification(detail: impl Into<String>) -> Self {
        Self::new(UpdateErrorKind::Verification, detail)
    }

    pub(super) fn storage(detail: impl Into<String>) -> Self {
        Self::new(UpdateErrorKind::Storage, detail)
    }

    pub(super) fn state(detail: impl Into<String>) -> Self {
        Self::new(UpdateErrorKind::State, detail)
    }

    pub(super) fn internal(detail: impl Into<String>) -> Self {
        Self::new(UpdateErrorKind::Internal, detail)
    }
}

impl std::fmt::Display for UpdateError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl std::error::Error for UpdateError {}

impl From<UpdateError> for ApplicationUpdateError {
    fn from(error: UpdateError) -> Self {
        let kind = match error.kind {
            UpdateErrorKind::Policy => ApplicationUpdateErrorKind::Unavailable,
            UpdateErrorKind::Network => ApplicationUpdateErrorKind::Network,
            UpdateErrorKind::Verification => ApplicationUpdateErrorKind::Verification,
            UpdateErrorKind::Storage => ApplicationUpdateErrorKind::Storage,
            UpdateErrorKind::Authorization => ApplicationUpdateErrorKind::Authorization,
            UpdateErrorKind::Installation => ApplicationUpdateErrorKind::Installation,
            UpdateErrorKind::State => ApplicationUpdateErrorKind::State,
            UpdateErrorKind::Internal => ApplicationUpdateErrorKind::Internal,
        };
        ApplicationUpdateError::new(kind, error.detail)
    }
}

struct UpdateService {
    current_version: Version,
    current_exe: PathBuf,
    cache_directory: PathBuf,
    installation: Installation,
    network: crate::network::NetworkService,
    config_store: ApplicationConfigStore,
    prepared: Option<PreparedUpdate>,
}

impl UpdateService {
    fn from_environment(
        environment: &AppEnvironment,
        config_store: ApplicationConfigStore,
        network: crate::network::NetworkService,
    ) -> Result<Self, UpdateError> {
        if !environment.updates.enabled {
            return Err(UpdateError::policy(environment.updates.detail.clone()));
        }
        if running_as_root() {
            return Err(UpdateError::policy(
                "Automatic updates are disabled when Dogi runs as root",
            ));
        }
        let current_version = Version::parse(env!("CARGO_PKG_VERSION"))
            .map_err(|_| UpdateError::internal("the Dogi build version is invalid"))?;
        let current_exe = environment.executable.clone();
        let installation =
            Installation::for_environment(environment).map_err(UpdateError::policy)?;
        let cache_directory = environment.paths.update_cache();
        Ok(Self {
            current_version,
            current_exe,
            cache_directory,
            installation,
            network,
            config_store,
            prepared: None,
        })
    }

    fn manage(
        &mut self,
        operation: ApplicationUpdateOperation,
    ) -> Result<ApplicationUpdateResult, UpdateError> {
        match operation {
            ApplicationUpdateOperation::Prepare(intent) => self.prepare(intent),
            ApplicationUpdateOperation::Install => self.install(),
        }
    }

    fn prepare(
        &mut self,
        intent: ApplicationUpdateCheckIntent,
    ) -> Result<ApplicationUpdateResult, UpdateError> {
        if intent == ApplicationUpdateCheckIntent::Automatic
            && !self
                .config_store
                .automatic_update_check_due(SystemTime::now(), AUTOMATIC_CHECK_INTERVAL)
                .map_err(|error| UpdateError::state(error.to_string()))?
        {
            return Ok(ApplicationUpdateResult::Deferred);
        }
        let network = self
            .network
            .policy()
            .map_err(|error| UpdateError::network(error.to_string()))?;
        let github = GitHubReleaseClient::new(&network);
        let kind = self.installation.asset_kind();
        let Some(candidate) = github.latest(&self.current_version, &kind)? else {
            self.prepared = None;
            self.record_successful_check();
            return Ok(ApplicationUpdateResult::UpToDate);
        };
        let download = github.download(&candidate, &self.cache_directory)?;
        let version = candidate.version.to_string();
        self.prepared = Some(PreparedUpdate {
            candidate,
            download,
        });
        self.record_successful_check();
        Ok(ApplicationUpdateResult::Ready { version })
    }

    fn record_successful_check(&self) {
        let _ = self
            .config_store
            .record_successful_update_check(SystemTime::now());
    }

    fn install(&mut self) -> Result<ApplicationUpdateResult, UpdateError> {
        let prepared = self
            .prepared
            .as_ref()
            .ok_or_else(|| UpdateError::state("no verified Dogi update is ready to install"))?;
        match self
            .installation
            .install(&prepared.download, &prepared.candidate, &self.current_exe)
        {
            Ok(InstallationOutcome { runtime_warning }) => {
                Ok(ApplicationUpdateResult::InstalledNeedsRestart {
                    version: prepared.candidate.version.to_string(),
                    detail: runtime_warning.unwrap_or_default(),
                })
            }
            Err(InstallationError::Cancelled) => Ok(ApplicationUpdateResult::Cancelled),
            Err(InstallationError::Verification(detail)) => Err(UpdateError::verification(detail)),
            Err(InstallationError::Authorization(detail)) => {
                Err(UpdateError::new(UpdateErrorKind::Authorization, detail))
            }
            Err(InstallationError::Failed(detail)) => {
                Err(UpdateError::new(UpdateErrorKind::Installation, detail))
            }
        }
    }

    fn schedule_relaunch(&self) -> Result<(), UpdateError> {
        install::schedule_relaunch_after_exit(&self.current_exe)
            .map_err(|detail| UpdateError::new(UpdateErrorKind::Installation, detail))
    }
}

pub(crate) fn application_update_manager(
    environment: &AppEnvironment,
    config_store: ApplicationConfigStore,
    network: crate::network::NetworkService,
) -> ApplicationUpdateManager {
    let current_version = env!("CARGO_PKG_VERSION").to_owned();
    let service = match UpdateService::from_environment(environment, config_store, network) {
        Ok(service) => service,
        Err(error) => return ApplicationUpdateManager::unavailable(error.to_string()),
    };
    let service = Arc::new(Mutex::new(service));
    let manage_service = Arc::clone(&service);
    ApplicationUpdateManager {
        supported: true,
        current_version,
        detail: String::new(),
        manage: Arc::new(move |operation| {
            let mut service = manage_service.lock().map_err(|_| {
                ApplicationUpdateError::new(
                    ApplicationUpdateErrorKind::Internal,
                    "the update manager stopped unexpectedly",
                )
            })?;
            service
                .manage(operation)
                .map_err(ApplicationUpdateError::from)
        }),
        relaunch_after_exit: {
            let service = Arc::clone(&service);
            Arc::new(move || {
                let service = service.lock().map_err(|_| {
                    ApplicationUpdateError::new(
                        ApplicationUpdateErrorKind::Internal,
                        "the update manager stopped unexpectedly",
                    )
                })?;
                service
                    .schedule_relaunch()
                    .map_err(ApplicationUpdateError::from)
            })
        },
        notify_ready: Arc::new(crate::desktop::notifications::show_update_ready),
    }
}

pub(crate) fn run_internal_command() -> Option<std::process::ExitCode> {
    install::run_internal_command()
}

#[cfg(unix)]
fn running_as_root() -> bool {
    // SAFETY: `geteuid` has no preconditions and does not dereference pointers.
    unsafe { libc::geteuid() == 0 }
}

#[cfg(not(unix))]
fn running_as_root() -> bool {
    false
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    #[test]
    fn development_policy_is_enforced_before_network_setup() {
        let environment = AppEnvironment::detect().unwrap();
        let store = ApplicationConfigStore::for_environment(&environment);
        let network = crate::network::NetworkService::new(store.clone());
        assert!(UpdateService::from_environment(&environment, store, network).is_err());
    }
}

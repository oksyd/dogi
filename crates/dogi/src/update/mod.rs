pub(crate) mod github;
mod install;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use dogi_core::DogiError;
use dogi_ui::{
    ApplicationUpdateCheckIntent, ApplicationUpdateManager, ApplicationUpdateOperation,
    ApplicationUpdateResult,
};
use semver::Version;

use self::github::{GitHubReleaseClient, ReleaseCandidate};
use self::install::{Installation, InstallationError};
use crate::config::application::ApplicationConfigStore;
use crate::environment::AppEnvironment;

const AUTOMATIC_CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

struct PreparedUpdate {
    candidate: ReleaseCandidate,
    artifact: PathBuf,
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
    ) -> std::result::Result<Self, String> {
        if !environment.updates.enabled {
            return Err(environment.updates.detail.clone());
        }
        if running_as_root() {
            return Err("Automatic updates are disabled when Dogi runs as root".to_owned());
        }
        let current_version = Version::parse(env!("CARGO_PKG_VERSION"))
            .map_err(|_| "the Dogi build version is invalid".to_owned())?;
        let current_exe = environment.executable.clone();
        let installation = Installation::for_environment(environment)?;
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
    ) -> std::result::Result<ApplicationUpdateResult, String> {
        match operation {
            ApplicationUpdateOperation::Prepare(intent) => self.prepare(intent),
            ApplicationUpdateOperation::Install => self.install(),
        }
    }

    fn prepare(
        &mut self,
        intent: ApplicationUpdateCheckIntent,
    ) -> std::result::Result<ApplicationUpdateResult, String> {
        if intent == ApplicationUpdateCheckIntent::Automatic
            && !self
                .config_store
                .automatic_update_check_due(SystemTime::now(), AUTOMATIC_CHECK_INTERVAL)
                .map_err(|error| error.to_string())?
        {
            return Ok(ApplicationUpdateResult::Deferred);
        }
        let network = self.network.policy().map_err(|error| error.to_string())?;
        let github = GitHubReleaseClient::new(&network);
        let kind = self.installation.asset_kind();
        let Some(candidate) = github.latest(&self.current_version, &kind)? else {
            self.prepared = None;
            self.record_successful_check();
            return Ok(ApplicationUpdateResult::UpToDate);
        };
        let artifact = github.download(&candidate.artifact, &self.cache_directory)?;
        let version = candidate.version.to_string();
        self.prepared = Some(PreparedUpdate {
            candidate,
            artifact,
        });
        self.record_successful_check();
        Ok(ApplicationUpdateResult::Ready { version })
    }

    fn record_successful_check(&self) {
        let _ = self
            .config_store
            .record_successful_update_check(SystemTime::now());
    }

    fn install(&mut self) -> std::result::Result<ApplicationUpdateResult, String> {
        let prepared = self
            .prepared
            .as_ref()
            .ok_or_else(|| "no verified Dogi update is ready to install".to_owned())?;
        match self.installation.install_and_restart(
            &prepared.artifact,
            &prepared.candidate.version,
            &self.current_exe,
        ) {
            Ok(()) => Ok(ApplicationUpdateResult::Restarting),
            Err(InstallationError::Cancelled) => Ok(ApplicationUpdateResult::Cancelled),
            Err(InstallationError::Failed(detail)) => Err(detail),
        }
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
        Err(detail) => return ApplicationUpdateManager::unavailable(detail),
    };
    let service = Arc::new(Mutex::new(service));
    ApplicationUpdateManager {
        supported: true,
        current_version,
        detail: String::new(),
        manage: Arc::new(move |operation| {
            let mut service = service.lock().map_err(|_| {
                DogiError::BackendUnavailable("the update manager stopped unexpectedly".to_owned())
            })?;
            service
                .manage(operation)
                .map_err(DogiError::BackendUnavailable)
        }),
        notify_ready: Arc::new(crate::desktop::notifications::show_update_ready),
    }
}

#[cfg(unix)]
fn running_as_root() -> bool {
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

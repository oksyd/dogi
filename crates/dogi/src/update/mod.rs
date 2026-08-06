mod github;
mod install;

use std::env;
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
    github: GitHubReleaseClient,
    config_store: Option<ApplicationConfigStore>,
    prepared: Option<PreparedUpdate>,
}

impl UpdateService {
    fn from_environment(
        config_store: Option<ApplicationConfigStore>,
    ) -> std::result::Result<Self, String> {
        if cfg!(debug_assertions) {
            return Err("Automatic updates are disabled for development builds".to_owned());
        }
        if running_as_root() {
            return Err("Automatic updates are disabled when Dogi runs as root".to_owned());
        }
        let current_version = Version::parse(env!("CARGO_PKG_VERSION"))
            .map_err(|_| "the Dogi build version is invalid".to_owned())?;
        let current_exe = env::current_exe()
            .and_then(std::fs::canonicalize)
            .map_err(|error| format!("could not identify the Dogi executable: {error}"))?;
        let installation = Installation::detect(&current_exe)?;
        let cache_directory = update_cache_directory()?.join("updates");
        Ok(Self {
            current_version,
            current_exe,
            cache_directory,
            installation,
            github: GitHubReleaseClient::new(),
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
            && let Some(store) = &self.config_store
            && !store
                .automatic_update_check_due(SystemTime::now(), AUTOMATIC_CHECK_INTERVAL)
                .map_err(|error| error.to_string())?
        {
            return Ok(ApplicationUpdateResult::Deferred);
        }
        let kind = self.installation.asset_kind();
        let Some(candidate) = self.github.latest(&self.current_version, &kind)? else {
            self.prepared = None;
            self.record_successful_check();
            return Ok(ApplicationUpdateResult::UpToDate);
        };
        let artifact = self
            .github
            .download(&candidate.artifact, &self.cache_directory)?;
        let version = candidate.version.to_string();
        self.prepared = Some(PreparedUpdate {
            candidate,
            artifact,
        });
        self.record_successful_check();
        Ok(ApplicationUpdateResult::Ready { version })
    }

    fn record_successful_check(&self) {
        if let Some(store) = &self.config_store {
            let _ = store.record_successful_update_check(SystemTime::now());
        }
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
    config_store: Option<ApplicationConfigStore>,
) -> ApplicationUpdateManager {
    let current_version = env!("CARGO_PKG_VERSION").to_owned();
    let service = match UpdateService::from_environment(config_store) {
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

fn update_cache_directory() -> std::result::Result<PathBuf, String> {
    if let Some(path) = env::var_os("XDG_CACHE_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path).join("dogi"));
    }
    crate::desktop::context::current_user()
        .map(|context| context.home.join(".cache/dogi"))
        .map_err(|error| error.to_string())
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
    fn development_builds_cannot_construct_an_update_service() {
        assert!(UpdateService::from_environment(None).is_err());
    }
}

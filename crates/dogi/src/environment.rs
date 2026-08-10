use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use dogi_core::{DogiError, Result};

use crate::desktop::{UserContext, context as desktop};

const DEBIAN_EXECUTABLE: &str = "/usr/bin/dogi";
const DEBIAN_DISTRIBUTION_MARKER: &str = "/usr/lib/dogi/distribution";
const PORTABLE_DISTRIBUTION_MARKER: &str = "share/dogi/distribution";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BuildChannel {
    Stable,
    Development,
}

impl BuildChannel {
    fn current() -> Self {
        match env!("DOGI_BUILD_CHANNEL") {
            "stable" => Self::Stable,
            _ => Self::Development,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Development => "development",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Distribution {
    Debian,
    Portable { root: PathBuf },
    Unmanaged,
}

impl Distribution {
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::Debian => "debian",
            Self::Portable { .. } => "portable",
            Self::Unmanaged => "unmanaged",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RuntimeIntegration {
    DebianSystemd,
    PortableSystemd,
    ForegroundOnly,
}

impl RuntimeIntegration {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::DebianSystemd => "debian-systemd",
            Self::PortableSystemd => "portable-systemd",
            Self::ForegroundOnly => "foreground-only",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RuntimePolicy {
    pub(crate) integration: RuntimeIntegration,
    pub(crate) management_detail: String,
}

impl RuntimePolicy {
    pub(crate) fn persistent_management_supported(&self) -> bool {
        !matches!(self.integration, RuntimeIntegration::ForegroundOnly)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct UpdatePolicy {
    pub(crate) enabled: bool,
    pub(crate) detail: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AppPaths {
    pub(crate) config: PathBuf,
    pub(crate) cache: PathBuf,
    pub(crate) session_runtime: PathBuf,
    pub(crate) runtime: PathBuf,
    pub(crate) global_runtime_lock: PathBuf,
}

impl AppPaths {
    pub(crate) fn application_config(&self) -> PathBuf {
        self.config.join("config.json")
    }

    pub(crate) fn device_settings(&self) -> PathBuf {
        self.config.join("master3s.json")
    }

    pub(crate) fn device_transaction(&self) -> PathBuf {
        self.config.join("device-transaction.json")
    }

    pub(crate) fn device_transaction_lock(&self) -> PathBuf {
        self.runtime.join("device-transaction.lock")
    }

    pub(crate) fn update_cache(&self) -> PathBuf {
        self.cache.join("updates")
    }

    pub(crate) fn battery_notification_state(&self) -> PathBuf {
        self.cache.join("battery-notifications.json")
    }

    pub(crate) fn runtime_control_socket(&self) -> PathBuf {
        self.runtime.join("runtime-control.sock")
    }

    pub(crate) fn gui_instance_lock(&self) -> PathBuf {
        self.runtime.join("gui.lock")
    }
}

#[derive(Clone, Debug)]
pub(crate) struct AppEnvironment {
    pub(crate) channel: BuildChannel,
    pub(crate) distribution: Distribution,
    pub(crate) executable: PathBuf,
    pub(crate) user: UserContext,
    pub(crate) paths: AppPaths,
    pub(crate) runtime: RuntimePolicy,
    pub(crate) updates: UpdatePolicy,
}

impl AppEnvironment {
    pub(crate) fn detect() -> Result<Self> {
        let channel = BuildChannel::current();
        let executable = env::current_exe()
            .and_then(fs::canonicalize)
            .map_err(|error| DogiError::Config(format!("failed to identify Dogi: {error}")))?;
        let user = desktop::current_user()?;
        let distribution = detect_distribution(channel, &executable);
        let paths = resolve_paths(channel, &user)?;
        let runtime = runtime_policy(channel, &distribution);
        let updates = update_policy(channel, &distribution);

        Ok(Self {
            channel,
            distribution,
            executable,
            user,
            paths,
            runtime,
            updates,
        })
    }

    pub(crate) fn is_development(&self) -> bool {
        self.channel == BuildChannel::Development
    }

    pub(crate) fn default_background_operations_enabled(&self) -> bool {
        matches!(self.runtime.integration, RuntimeIntegration::DebianSystemd)
    }
}

fn detect_distribution(channel: BuildChannel, executable: &Path) -> Distribution {
    if channel == BuildChannel::Development {
        return Distribution::Unmanaged;
    }
    if executable == Path::new(DEBIAN_EXECUTABLE)
        && marker_has_value(Path::new(DEBIAN_DISTRIBUTION_MARKER), "debian")
    {
        return Distribution::Debian;
    }
    portable_root(executable).map_or(Distribution::Unmanaged, |root| Distribution::Portable {
        root,
    })
}

fn portable_root(executable: &Path) -> Option<PathBuf> {
    let bin = executable.parent()?;
    if bin.file_name()? != "bin" || executable.file_name()? != "dogi" {
        return None;
    }
    let root = bin.parent()?;
    marker_has_value(&root.join(PORTABLE_DISTRIBUTION_MARKER), "portable").then(|| root.to_owned())
}

fn marker_has_value(path: &Path, expected: &str) -> bool {
    fs::read_to_string(path).is_ok_and(|value| value.trim() == expected)
}

fn resolve_paths(channel: BuildChannel, user: &UserContext) -> Result<AppPaths> {
    let namespace = match channel {
        BuildChannel::Stable => "dogi",
        BuildChannel::Development => "dogi-development",
    };
    let config_home = user_scoped_path(user, "XDG_CONFIG_HOME", ".config");
    let cache_home = user_scoped_path(user, "XDG_CACHE_HOME", ".cache");
    let runtime_home = runtime_home(user)?;

    Ok(AppPaths {
        config: config_home.join(namespace),
        cache: cache_home.join(namespace),
        session_runtime: runtime_home.clone(),
        runtime: runtime_home.join(namespace),
        global_runtime_lock: runtime_home.join("dogi-runtime.lock"),
    })
}

fn user_scoped_path(user: &UserContext, variable: &str, fallback: &str) -> PathBuf {
    if user.uid.is_none()
        && let Some(path) = desktop::env_path(variable)
    {
        return path;
    }
    user.home.join(fallback)
}

fn runtime_home(user: &UserContext) -> Result<PathBuf> {
    if let Some(uid) = user.uid {
        return Ok(PathBuf::from(format!("/run/user/{uid}")));
    }
    if let Some(path) = desktop::env_path("XDG_RUNTIME_DIR") {
        return Ok(path);
    }
    #[cfg(unix)]
    {
        Ok(PathBuf::from(format!("/run/user/{}", unsafe {
            libc::geteuid()
        })))
    }
    #[cfg(not(unix))]
    {
        Err(DogiError::BackendUnavailable(
            "XDG_RUNTIME_DIR is not set for the desktop session".to_owned(),
        ))
    }
}

fn runtime_policy(channel: BuildChannel, distribution: &Distribution) -> RuntimePolicy {
    let (integration, detail) = match (channel, distribution) {
        (BuildChannel::Development, _) => (
            RuntimeIntegration::ForegroundOnly,
            "Development builds use an explicit foreground runtime and never modify the installed background service",
        ),
        (BuildChannel::Stable, Distribution::Debian) => (
            RuntimeIntegration::DebianSystemd,
            "Managed by the system-installed Dogi user service",
        ),
        (BuildChannel::Stable, Distribution::Portable { .. }) => (
            RuntimeIntegration::PortableSystemd,
            "Managed by an explicitly enabled portable Dogi user service",
        ),
        (BuildChannel::Stable, Distribution::Unmanaged) => (
            RuntimeIntegration::ForegroundOnly,
            "Persistent background integration is unavailable for an unmanaged Dogi binary",
        ),
    };
    RuntimePolicy {
        integration,
        management_detail: detail.to_owned(),
    }
}

fn update_policy(channel: BuildChannel, distribution: &Distribution) -> UpdatePolicy {
    match (channel, distribution) {
        (BuildChannel::Development, _) => UpdatePolicy {
            enabled: false,
            detail: "Automatic updates are disabled for development builds".to_owned(),
        },
        (BuildChannel::Stable, Distribution::Debian | Distribution::Portable { .. }) => {
            UpdatePolicy {
                enabled: true,
                detail: String::new(),
            }
        }
        (BuildChannel::Stable, Distribution::Unmanaged) => UpdatePolicy {
            enabled: false,
            detail: "Automatic updates require a Dogi Debian or portable installation".to_owned(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn development_paths_have_an_isolated_namespace() {
        let user = UserContext {
            home: PathBuf::from("/home/tester"),
            uid: Some(1000),
            gid: Some(1000),
        };

        let paths = resolve_paths(BuildChannel::Development, &user).unwrap();

        assert_eq!(
            paths.config,
            Path::new("/home/tester/.config/dogi-development")
        );
        assert_eq!(
            paths.cache,
            Path::new("/home/tester/.cache/dogi-development")
        );
        assert_eq!(paths.runtime, Path::new("/run/user/1000/dogi-development"));
        assert_eq!(
            paths.global_runtime_lock,
            Path::new("/run/user/1000/dogi-runtime.lock")
        );
    }

    #[test]
    fn development_builds_ignore_installation_layouts() {
        assert_eq!(
            detect_distribution(BuildChannel::Development, Path::new("/usr/bin/dogi")),
            Distribution::Unmanaged
        );
    }

    #[test]
    fn unmanaged_stable_binaries_do_not_gain_persistent_integrations() {
        let runtime = runtime_policy(BuildChannel::Stable, &Distribution::Unmanaged);
        let updates = update_policy(BuildChannel::Stable, &Distribution::Unmanaged);

        assert_eq!(runtime.integration, RuntimeIntegration::ForegroundOnly);
        assert!(!updates.enabled);
    }
}

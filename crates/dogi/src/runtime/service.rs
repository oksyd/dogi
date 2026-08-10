use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::FileTypeExt;

use dogi_core::{DogiError, Result};
use dogi_ui::{DesktopRuntimeOperation, DesktopRuntimePauseReason, DesktopRuntimeStatus};

use crate::desktop::UserContext;
use crate::environment::{AppEnvironment, RuntimeIntegration};

use super::{UINPUT_PATH, session};

const SERVICE_NAME: &str = "dogi-runtime.service";
const VENDOR_UNIT_PATH: &str = "/usr/lib/systemd/user/dogi-runtime.service";
const USER_UNIT_RELATIVE_PATH: &str = ".config/systemd/user/dogi-runtime.service";
const STATIC_DEBIAN_UNIT: &str = include_str!("../../assets/linux/dogi-runtime.service");
const PORTABLE_UNIT_TEMPLATE: &str = include_str!("../../assets/linux/dogi-runtime.service.in");
const EXECUTABLE_PLACEHOLDER: &str = "@DOGI_EXECUTABLE@";

pub(crate) fn manage(
    environment: &AppEnvironment,
    operation: DesktopRuntimeOperation,
) -> Result<DesktopRuntimeStatus> {
    ensure_persistent_integration(environment)?;
    match operation {
        DesktopRuntimeOperation::Reconcile { enabled: true } => ensure_running(environment),
        DesktopRuntimeOperation::Restart => restart(environment),
        DesktopRuntimeOperation::Reconcile { enabled: false } => stop(environment),
    }
}

pub(crate) fn current_pause_reason() -> DesktopRuntimePauseReason {
    pause_reason(session::inspect().mode)
}

fn ensure_running(environment: &AppEnvironment) -> Result<DesktopRuntimeStatus> {
    let context = &environment.user;
    ensure_user_manager(environment)?;
    validate_integration(environment)?;
    let unit_changed = if matches!(
        environment.runtime.integration,
        RuntimeIntegration::PortableSystemd
    ) {
        install_portable_unit(context, &portable_unit(&environment.executable))?
    } else {
        false
    };
    if unit_changed
        || matches!(
            environment.runtime.integration,
            RuntimeIntegration::DebianSystemd
        )
    {
        systemctl(context, &["daemon-reload"], "reload the Dogi runtime")?;
    }
    systemctl(
        context,
        &["enable", SERVICE_NAME],
        "enable the Dogi runtime",
    )?;
    systemctl(
        context,
        &[if unit_changed { "restart" } else { "start" }, SERVICE_NAME],
        "start the Dogi runtime",
    )?;

    let status = service_status(environment);
    if !status.active {
        return Err(DogiError::BackendUnavailable(service_failure_detail(
            context,
        )));
    }
    Ok(status)
}

fn restart(environment: &AppEnvironment) -> Result<DesktopRuntimeStatus> {
    let status = ensure_running(environment)?;
    systemctl(
        &environment.user,
        &["restart", SERVICE_NAME],
        "restart the Dogi runtime",
    )?;
    Ok(DesktopRuntimeStatus {
        active: true,
        ..status
    })
}

fn stop(environment: &AppEnvironment) -> Result<DesktopRuntimeStatus> {
    let context = &environment.user;
    ensure_user_manager(environment)?;
    let status = service_status(environment);
    if status.enabled || status.active {
        systemctl(
            context,
            &["disable", "--now", SERVICE_NAME],
            "stop the Dogi runtime",
        )?;
    }
    Ok(service_status(environment))
}

fn service_status(environment: &AppEnvironment) -> DesktopRuntimeStatus {
    let context = &environment.user;
    let enabled = systemctl_succeeds(context, &["is-enabled", "--quiet", SERVICE_NAME]);
    let active = systemctl_succeeds(context, &["is-active", "--quiet", SERVICE_NAME]);
    let session = session::inspect();
    let pause_reason = if active {
        pause_reason(session.mode)
    } else {
        DesktopRuntimePauseReason::None
    };
    let paused = pause_reason != DesktopRuntimePauseReason::None;
    let uinput_error = (active && !paused)
        .then(|| uinput_access(context).err().map(|error| error.to_string()))
        .flatten();
    let detail = if paused {
        session.detail
    } else {
        uinput_error.unwrap_or_default()
    };

    DesktopRuntimeStatus {
        enabled,
        active,
        ready: active && !paused && detail.is_empty(),
        paused,
        pause_reason,
        app_profiles_supported: app_profiles_supported(context),
        detail,
    }
}

const fn pause_reason(mode: session::GraphicalSessionMode) -> DesktopRuntimePauseReason {
    match mode {
        session::GraphicalSessionMode::LocalActive => DesktopRuntimePauseReason::None,
        session::GraphicalSessionMode::LocalLocked => DesktopRuntimePauseReason::DesktopLocked,
        session::GraphicalSessionMode::RemoteOnly => DesktopRuntimePauseReason::RemoteLogin,
        session::GraphicalSessionMode::Inactive => DesktopRuntimePauseReason::NoLocalDesktop,
        session::GraphicalSessionMode::Unknown => DesktopRuntimePauseReason::Unknown,
    }
}

pub(crate) fn print_unit(environment: &AppEnvironment) -> Result<()> {
    ensure_persistent_integration(environment)?;
    match environment.runtime.integration {
        RuntimeIntegration::DebianSystemd => print!("{STATIC_DEBIAN_UNIT}"),
        RuntimeIntegration::PortableSystemd => {
            print!("{}", portable_unit(&environment.executable));
        }
        RuntimeIntegration::ForegroundOnly => {
            return Err(DogiError::BackendUnavailable(
                environment.runtime.management_detail.clone(),
            ));
        }
    }
    Ok(())
}

pub(crate) fn install(environment: &AppEnvironment) -> Result<()> {
    let status = ensure_running(environment)?;
    println!("Dogi desktop runtime is enabled and running.");
    if !status.ready {
        eprintln!("Custom actions need attention: {}", status.detail);
    }
    Ok(())
}

pub(crate) fn uninstall(environment: &AppEnvironment) -> Result<()> {
    ensure_persistent_integration(environment)?;
    let context = &environment.user;
    if manager_socket(environment).is_ok() {
        let status = service_status(environment);
        if status.enabled || status.active {
            systemctl(
                context,
                &["disable", "--now", SERVICE_NAME],
                "stop the Dogi runtime",
            )?;
        }
    }

    if matches!(
        environment.runtime.integration,
        RuntimeIntegration::DebianSystemd
    ) {
        remove_known_user_override(environment)?;
        println!("Dogi desktop runtime is disabled. The package-owned unit was preserved.");
        return Ok(());
    }

    let unit_path = user_unit_path(context);
    match fs::read_to_string(&unit_path) {
        Ok(contents) if contents.starts_with("# Generated by Dogi portable integration.") => {
            fs::remove_file(&unit_path).map_err(|error| {
                DogiError::Config(format!("failed to remove {}: {error}", unit_path.display()))
            })?;
            if manager_socket(environment).is_ok() {
                systemctl(
                    context,
                    &["daemon-reload"],
                    "reload the user service manager",
                )?;
            }
            println!("Removed {}.", unit_path.display());
            Ok(())
        }
        Ok(_) => Err(DogiError::Config(format!(
            "refusing to remove an unmanaged service unit at {}",
            unit_path.display()
        ))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            println!("Dogi portable runtime integration is not installed.");
            Ok(())
        }
        Err(error) => Err(DogiError::Config(format!(
            "failed to read {}: {error}",
            unit_path.display()
        ))),
    }
}

fn remove_known_user_override(environment: &AppEnvironment) -> Result<()> {
    let context = &environment.user;
    let path = user_unit_path(context);
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(DogiError::Config(format!(
                "failed to read {}: {error}",
                path.display()
            )));
        }
    };
    if !contents.starts_with("# Generated by Dogi portable integration.")
        && !contents.starts_with("# Generated by Dogi. Manual changes will be replaced.")
    {
        return Err(DogiError::Config(format!(
            "refusing to remove an unmanaged service unit at {}",
            path.display()
        )));
    }
    fs::remove_file(&path).map_err(|error| {
        DogiError::Config(format!("failed to remove {}: {error}", path.display()))
    })?;
    if manager_socket(environment).is_ok() {
        systemctl(
            context,
            &["daemon-reload"],
            "reload the user service manager",
        )?;
    }
    println!("Removed obsolete user override {}.", path.display());
    Ok(())
}

pub(crate) fn path(environment: &AppEnvironment) -> Result<PathBuf> {
    ensure_persistent_integration(environment)?;
    Ok(match environment.runtime.integration {
        RuntimeIntegration::DebianSystemd => PathBuf::from(VENDOR_UNIT_PATH),
        RuntimeIntegration::PortableSystemd => user_unit_path(&environment.user),
        RuntimeIntegration::ForegroundOnly => {
            return Err(DogiError::BackendUnavailable(
                environment.runtime.management_detail.clone(),
            ));
        }
    })
}

fn ensure_persistent_integration(environment: &AppEnvironment) -> Result<()> {
    if environment.user.uid.is_some() {
        return Err(DogiError::InvalidArgument(
            "manage the Dogi user service as the desktop user, without sudo".to_owned(),
        ));
    }
    if environment.runtime.persistent_management_supported() {
        Ok(())
    } else {
        Err(DogiError::BackendUnavailable(
            environment.runtime.management_detail.clone(),
        ))
    }
}

fn validate_integration(environment: &AppEnvironment) -> Result<()> {
    let user_unit = user_unit_path(&environment.user);
    match environment.runtime.integration {
        RuntimeIntegration::DebianSystemd => {
            if !Path::new(VENDOR_UNIT_PATH).is_file() {
                return Err(DogiError::Config(
                    "the Debian package-owned Dogi runtime unit is missing".to_owned(),
                ));
            }
            if user_unit.exists() {
                return Err(DogiError::Config(format!(
                    "{} overrides the package-owned Dogi runtime; remove that user unit before enabling the packaged service",
                    user_unit.display()
                )));
            }
        }
        RuntimeIntegration::PortableSystemd => {
            if Path::new(VENDOR_UNIT_PATH).exists() {
                return Err(DogiError::Config(
                    "portable runtime integration cannot override an installed Dogi Debian service"
                        .to_owned(),
                ));
            }
            if let Ok(contents) = fs::read_to_string(&user_unit)
                && !contents.starts_with("# Generated by Dogi portable integration.")
            {
                return Err(DogiError::Config(format!(
                    "refusing to overwrite an unmanaged service unit at {}",
                    user_unit.display()
                )));
            }
        }
        RuntimeIntegration::ForegroundOnly => ensure_persistent_integration(environment)?,
    }
    Ok(())
}

fn user_unit_path(context: &UserContext) -> PathBuf {
    context.home.join(USER_UNIT_RELATIVE_PATH)
}

fn install_portable_unit(context: &UserContext, contents: &str) -> Result<bool> {
    let path = user_unit_path(context);
    if fs::read_to_string(&path).is_ok_and(|installed| installed == contents) {
        return Ok(false);
    }
    let parent = path.parent().ok_or_else(|| {
        DogiError::Config(format!("service path has no parent: {}", path.display()))
    })?;
    fs::create_dir_all(parent).map_err(|error| {
        DogiError::Config(format!("failed to create {}: {error}", parent.display()))
    })?;
    let temporary = path.with_extension(format!("service.tmp-{}", std::process::id()));
    fs::write(&temporary, contents).map_err(|error| {
        DogiError::Config(format!("failed to write {}: {error}", temporary.display()))
    })?;
    fs::rename(&temporary, &path).map_err(|error| {
        DogiError::Config(format!("failed to replace {}: {error}", path.display()))
    })?;
    Ok(true)
}

fn portable_unit(binary: &Path) -> String {
    PORTABLE_UNIT_TEMPLATE.replace(
        EXECUTABLE_PLACEHOLDER,
        &systemd_quote(&binary.display().to_string()),
    )
}

fn ensure_user_manager(environment: &AppEnvironment) -> Result<()> {
    manager_socket(environment).map(|_| ())
}

#[cfg(unix)]
fn manager_socket(environment: &AppEnvironment) -> Result<PathBuf> {
    let socket = environment.paths.session_runtime.join("systemd/private");
    match fs::metadata(&socket) {
        Ok(metadata) if metadata.file_type().is_socket() => Ok(socket),
        Ok(_) => Err(DogiError::BackendUnavailable(format!(
            "{} is not a systemd user-manager socket",
            socket.display()
        ))),
        Err(error) => Err(DogiError::BackendUnavailable(format!(
            "systemd user manager is unavailable at {}: {error}",
            socket.display()
        ))),
    }
}

#[cfg(not(unix))]
fn manager_socket(_environment: &AppEnvironment) -> Result<PathBuf> {
    Err(DogiError::BackendUnavailable(
        "Dogi desktop runtime requires a systemd user manager".to_owned(),
    ))
}

fn uinput_access(context: &UserContext) -> Result<()> {
    if context.uid.is_some() {
        return Err(DogiError::BackendUnavailable(
            "run the Dogi GUI as the desktop user, without sudo".to_owned(),
        ));
    }
    fs::OpenOptions::new()
        .write(true)
        .open(UINPUT_PATH)
        .map(drop)
        .map_err(|error| {
            DogiError::BackendUnavailable(format!(
                "cannot access {UINPUT_PATH}: {error}. Install Dogi's udev rules, then sign in again"
            ))
        })
}

fn app_profiles_supported(context: &UserContext) -> bool {
    env::var("XDG_SESSION_TYPE").is_ok_and(|session| session.eq_ignore_ascii_case("x11"))
        && context
            .run("xprop", &["-version"])
            .is_ok_and(|output| output.status.success())
}

fn systemctl(context: &UserContext, args: &[&str], operation: &str) -> Result<()> {
    let mut full_args = Vec::with_capacity(args.len() + 1);
    full_args.push("--user");
    full_args.extend_from_slice(args);
    let output = context.run("systemctl", &full_args).map_err(|error| {
        DogiError::BackendUnavailable(format!("failed to {operation}: {error}"))
    })?;
    if output.status.success() {
        Ok(())
    } else {
        Err(DogiError::BackendUnavailable(format!(
            "could not {operation}: {}",
            command_detail(&output)
        )))
    }
}

fn systemctl_succeeds(context: &UserContext, args: &[&str]) -> bool {
    let mut full_args = Vec::with_capacity(args.len() + 1);
    full_args.push("--user");
    full_args.extend_from_slice(args);
    context
        .run("systemctl", &full_args)
        .is_ok_and(|output| output.status.success())
}

fn service_failure_detail(context: &UserContext) -> String {
    context
        .run(
            "systemctl",
            &[
                "--user",
                "show",
                SERVICE_NAME,
                "--property=Result,ExecMainStatus",
                "--value",
            ],
        )
        .map(|output| command_detail(&output))
        .ok()
        .filter(|detail| !detail.is_empty())
        .unwrap_or_else(|| "Dogi desktop runtime did not remain active".to_owned())
}

fn command_detail(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if !stderr.is_empty() {
        return stderr;
    }
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn systemd_quote(value: &str) -> String {
    if value
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "/._:-".contains(character))
    {
        return value.to_owned();
    }
    let escaped = value
        .chars()
        .flat_map(|character| match character {
            '\\' => "\\\\".chars().collect::<Vec<_>>(),
            '"' => "\\\"".chars().collect::<Vec<_>>(),
            '%' => "%%".chars().collect::<Vec<_>>(),
            _ => vec![character],
        })
        .collect::<String>();
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_unit_is_fixed_and_hardened() {
        assert!(STATIC_DEBIAN_UNIT.contains(
            "ExecStart=/usr/bin/dogi runtime run --idle-timeout-ms 1000 --execute-actions --allow-device-write"
        ));
        assert!(STATIC_DEBIAN_UNIT.contains("Restart=on-failure"));
        assert!(STATIC_DEBIAN_UNIT.contains("RuntimeDirectory=dogi"));
        assert!(STATIC_DEBIAN_UNIT.contains("CacheDirectory=dogi"));
        assert!(STATIC_DEBIAN_UNIT.contains("ProtectSystem=strict"));
    }

    #[test]
    fn portable_unit_quotes_its_fixed_executable() {
        let service = portable_unit(Path::new("/opt/dogi bin/bin/dogi"));
        assert!(service.starts_with("# Generated by Dogi portable integration."));
        assert!(service.contains(
            "ExecStart=\"/opt/dogi bin/bin/dogi\" runtime run --idle-timeout-ms 1000 --execute-actions"
        ));
    }

    #[test]
    fn systemd_arguments_are_quoted_only_when_needed() {
        assert_eq!(systemd_quote("/usr/bin/dogi"), "/usr/bin/dogi");
        assert_eq!(systemd_quote("device 1"), "\"device 1\"");
        assert_eq!(systemd_quote("device\"1"), "\"device\\\"1\"");
        assert_eq!(systemd_quote("/opt/dogi%20/dogi"), "\"/opt/dogi%%20/dogi\"");
    }

    #[test]
    fn only_a_verified_local_session_has_no_pause_reason() {
        assert_eq!(
            pause_reason(session::GraphicalSessionMode::LocalActive),
            DesktopRuntimePauseReason::None
        );
        assert_eq!(
            pause_reason(session::GraphicalSessionMode::RemoteOnly),
            DesktopRuntimePauseReason::RemoteLogin
        );
        assert_eq!(
            pause_reason(session::GraphicalSessionMode::LocalLocked),
            DesktopRuntimePauseReason::DesktopLocked
        );
        assert_eq!(
            pause_reason(session::GraphicalSessionMode::Unknown),
            DesktopRuntimePauseReason::Unknown
        );
    }
}

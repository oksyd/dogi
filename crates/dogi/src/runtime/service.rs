use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

#[cfg(unix)]
use std::os::unix::fs::{FileTypeExt, chown};

use dogi_core::{DogiError, Result};
use dogi_ui::{DesktopRuntimeOperation, DesktopRuntimeStatus};

use crate::desktop::{UserContext, context as desktop};

use super::UINPUT_PATH;

const SERVICE_NAME: &str = "dogi-runtime.service";
const SERVICE_RELATIVE_PATH: &str = ".config/systemd/user/dogi-runtime.service";
const SERVICE_TEMPLATE: &str = include_str!("../../assets/linux/dogi-runtime.service.in");
const EXECUTABLE_PLACEHOLDER: &str = "@DOGI_EXECUTABLE@";
const REVISION_PLACEHOLDER: &str = "@DOGI_EXECUTABLE_REVISION@";

pub(crate) fn ensure_running() -> Result<DesktopRuntimeStatus> {
    let context = desktop::current_user()?;
    ensure_user_manager(&context)?;
    let uinput_error = uinput_access(&context).err().map(|error| error.to_string());

    let binary = env::current_exe().map_err(|error| {
        DogiError::Config(format!("failed to locate the Dogi executable: {error}"))
    })?;
    let contents = unit_for_binary(&binary)?;
    let unit_changed = install_unit(&context, &contents)?;

    if unit_changed {
        systemctl(&context, &["daemon-reload"], "reload the Dogi runtime")?;
    }
    systemctl(
        &context,
        &["enable", SERVICE_NAME],
        "enable the Dogi runtime",
    )?;
    systemctl(
        &context,
        &[if unit_changed { "restart" } else { "start" }, SERVICE_NAME],
        "start the Dogi runtime",
    )?;

    let status = service_status(&context);
    if !status.active {
        return Err(DogiError::BackendUnavailable(service_failure_detail(
            &context,
        )));
    }

    Ok(DesktopRuntimeStatus {
        ready: uinput_error.is_none(),
        detail: uinput_error.unwrap_or_default(),
        ..status
    })
}

pub(crate) fn manage(operation: DesktopRuntimeOperation) -> Result<DesktopRuntimeStatus> {
    match operation {
        DesktopRuntimeOperation::Reconcile { enabled: true } | DesktopRuntimeOperation::Restart => {
            ensure_running()
        }
        DesktopRuntimeOperation::Reconcile { enabled: false } => stop(),
    }
}

fn stop() -> Result<DesktopRuntimeStatus> {
    let context = desktop::current_user()?;
    ensure_user_manager(&context)?;

    let status = service_status(&context);
    if status.enabled || status.active {
        systemctl(
            &context,
            &["disable", "--now", SERVICE_NAME],
            "stop the Dogi runtime",
        )?;
    }

    Ok(service_status(&context))
}

fn service_status(context: &UserContext) -> DesktopRuntimeStatus {
    let enabled = systemctl_succeeds(context, &["is-enabled", "--quiet", SERVICE_NAME]);
    let active = systemctl_succeeds(context, &["is-active", "--quiet", SERVICE_NAME]);
    let uinput_error = active
        .then(|| uinput_access(context).err().map(|error| error.to_string()))
        .flatten();

    DesktopRuntimeStatus {
        enabled,
        active,
        ready: active && uinput_error.is_none(),
        app_profiles_supported: app_profiles_supported(context),
        detail: uinput_error.unwrap_or_default(),
    }
}

pub(crate) fn print_unit() -> Result<()> {
    let binary = env::current_exe().map_err(|error| {
        DogiError::Config(format!("failed to locate the Dogi executable: {error}"))
    })?;
    print!("{}", unit_for_binary(&binary)?);
    Ok(())
}

pub(crate) fn install() -> Result<()> {
    let status = ensure_running()?;
    println!("Dogi desktop runtime is installed and running.");
    if !status.ready {
        eprintln!("Custom actions need attention: {}", status.detail);
    }
    Ok(())
}

pub(crate) fn uninstall() -> Result<()> {
    let context = desktop::current_user()?;
    let unit_path = path_for(&context);

    if unit_path.exists() && manager_socket(&context).is_ok() {
        systemctl(
            &context,
            &["disable", "--now", SERVICE_NAME],
            "stop the Dogi runtime",
        )?;
    }

    match fs::remove_file(&unit_path) {
        Ok(()) => {
            if manager_socket(&context).is_ok() {
                systemctl(
                    &context,
                    &["daemon-reload"],
                    "reload the user service manager",
                )?;
            }
            println!("Removed {}.", unit_path.display());
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            println!("Dogi desktop runtime is not installed.");
            Ok(())
        }
        Err(error) => Err(DogiError::Config(format!(
            "failed to remove {}: {error}",
            unit_path.display()
        ))),
    }
}

pub(crate) fn path() -> Result<PathBuf> {
    Ok(path_for(&desktop::current_user()?))
}

fn path_for(context: &UserContext) -> PathBuf {
    context.home.join(SERVICE_RELATIVE_PATH)
}

fn ensure_user_manager(context: &UserContext) -> Result<()> {
    manager_socket(context).map(|_| ())
}

#[cfg(unix)]
fn manager_socket(context: &UserContext) -> Result<PathBuf> {
    let runtime_dir = context
        .uid
        .map(|uid| PathBuf::from(format!("/run/user/{uid}")))
        .or_else(|| desktop::env_path("XDG_RUNTIME_DIR"))
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe { libc::geteuid() })));
    let socket = runtime_dir.join("systemd/private");

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
fn manager_socket(_context: &UserContext) -> Result<PathBuf> {
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

fn install_unit(context: &UserContext, contents: &str) -> Result<bool> {
    let path = path_for(context);
    if fs::read_to_string(&path).is_ok_and(|installed| installed == contents) {
        return Ok(false);
    }

    let parent = path.parent().ok_or_else(|| {
        DogiError::Config(format!("service path has no parent: {}", path.display()))
    })?;
    let parent_arg = parent.to_string_lossy().into_owned();
    let output = context
        .run("mkdir", &["-p", &parent_arg])
        .map_err(|error| {
            DogiError::Config(format!(
                "failed to create service directory {}: {error}",
                parent.display()
            ))
        })?;
    if !output.status.success() {
        return Err(DogiError::Config(format!(
            "failed to create service directory {}: {}",
            parent.display(),
            command_detail(&output)
        )));
    }

    let temporary = path.with_extension(format!("service.tmp-{}", std::process::id()));
    fs::write(&temporary, contents).map_err(|error| {
        DogiError::Config(format!("failed to write {}: {error}", temporary.display()))
    })?;

    #[cfg(unix)]
    if let (Some(uid), Some(gid)) = (context.uid, context.gid) {
        chown(&temporary, Some(uid), Some(gid)).map_err(|error| {
            DogiError::Config(format!(
                "failed to preserve ownership of {}: {error}",
                temporary.display()
            ))
        })?;
    }

    fs::rename(&temporary, &path).map_err(|error| {
        DogiError::Config(format!(
            "failed to replace {} with {}: {error}",
            path.display(),
            temporary.display()
        ))
    })?;
    Ok(true)
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

fn unit_for_binary(binary: &Path) -> Result<String> {
    let metadata = fs::metadata(binary).map_err(|error| {
        DogiError::Config(format!(
            "failed to inspect Dogi executable {}: {error}",
            binary.display()
        ))
    })?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let revision = format!("{}-{modified}", metadata.len());
    Ok(build_unit(binary, &revision))
}

fn build_unit(binary: &Path, revision: &str) -> String {
    SERVICE_TEMPLATE
        .replace(
            EXECUTABLE_PLACEHOLDER,
            &systemd_quote(&binary.display().to_string()),
        )
        .replace(REVISION_PLACEHOLDER, revision)
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
    fn generated_unit_is_canonical_and_hardened() {
        let service = build_unit(Path::new("/usr/bin/dogi"), "test-revision");

        assert!(!service.contains(EXECUTABLE_PLACEHOLDER));
        assert!(!service.contains(REVISION_PLACEHOLDER));
        assert!(service.contains("# Executable-Revision: test-revision"));
        assert!(service.contains(
            "ExecStart=/usr/bin/dogi runtime run --idle-timeout-ms 1000 --execute-actions --allow-device-write"
        ));
        assert!(service.contains("Restart=on-failure"));
        assert!(service.contains("RuntimeDirectory=dogi"));
        assert!(service.contains("RuntimeDirectoryMode=0700"));
        assert!(service.contains("NoNewPrivileges=true"));
        assert!(service.contains("ProtectSystem=strict"));
        assert!(service.contains("WantedBy=default.target"));
    }

    #[test]
    fn generated_unit_quotes_executable_paths() {
        let service = build_unit(Path::new("/opt/dogi bin/dogi"), "revision");

        assert!(service.contains(
            "ExecStart=\"/opt/dogi bin/dogi\" runtime run --idle-timeout-ms 1000 --execute-actions"
        ));
    }

    #[test]
    fn systemd_arguments_are_quoted_only_when_needed() {
        assert_eq!(systemd_quote("/usr/bin/dogi"), "/usr/bin/dogi");
        assert_eq!(systemd_quote("device 1"), "\"device 1\"");
        assert_eq!(systemd_quote("device\"1"), "\"device\\\"1\"");
        assert_eq!(systemd_quote("/opt/dogi%20/dogi"), "\"/opt/dogi%%20/dogi\"");
    }
}

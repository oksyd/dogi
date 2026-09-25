use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use semver::Version;
use sha2::{Digest, Sha256};

use super::github::{MAX_ARTIFACT_SIZE, ReleaseAssetKind, ReleaseCandidate};
use crate::environment::{AppEnvironment, Distribution};

const DPKG_DEB: &str = "/usr/bin/dpkg-deb";
const PKEXEC: &str = "/usr/bin/pkexec";
const APT_GET: &str = "/usr/bin/apt-get";
const DPKG_QUERY: &str = "/usr/bin/dpkg-query";
const SYSTEMCTL: &str = "/usr/bin/systemctl";
const INTERNAL_DEBIAN_INSTALL: &str = "--dogi-internal-install-debian";
const INTERNAL_RELAUNCH: &str = "--dogi-internal-relaunch";
const HELPER_VERIFICATION_EXIT: u8 = 65;
const HELPER_INSTALLATION_EXIT: u8 = 70;
const MAX_ARCHIVE_ENTRIES: usize = 256;
const MAX_UNPACKED_SIZE: u64 = 256 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum InstallationError {
    Cancelled,
    Verification(String),
    Authorization(String),
    Failed(String),
}

impl From<String> for InstallationError {
    fn from(detail: String) -> Self {
        Self::Failed(detail)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct InstallationOutcome {
    pub(super) runtime_warning: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Installation {
    Debian { architecture: &'static str },
    Portable { root: PathBuf, target: &'static str },
}

impl Installation {
    pub(super) fn for_environment(environment: &AppEnvironment) -> Result<Self, String> {
        let (architecture, target) = platform_architecture()?;
        match &environment.distribution {
            Distribution::Debian => Ok(Self::Debian { architecture }),
            Distribution::Portable { root } => Ok(Self::Portable {
                root: root.clone(),
                target,
            }),
            Distribution::Unmanaged => Err(
                "automatic installation is available only for Dogi Debian and portable packages"
                    .to_owned(),
            ),
        }
    }

    pub(super) fn asset_kind(&self) -> ReleaseAssetKind {
        match self {
            Self::Debian { architecture } => ReleaseAssetKind::Debian { architecture },
            Self::Portable { target, .. } => ReleaseAssetKind::Portable { target },
        }
    }

    pub(super) fn install(
        &self,
        artifact: &Path,
        candidate: &ReleaseCandidate,
        current_exe: &Path,
    ) -> Result<InstallationOutcome, InstallationError> {
        let expected_name = self.asset_kind().expected_name(&candidate.version);
        if candidate.artifact.name != expected_name
            || artifact.file_name() != Some(OsStr::new(&expected_name))
        {
            return Err(InstallationError::Verification(
                "the downloaded package does not match the selected release".to_owned(),
            ));
        }
        let mut warnings = Vec::new();
        match self {
            Self::Debian { architecture } => {
                install_debian(artifact, candidate, architecture, current_exe)?;
            }
            Self::Portable { root, target } => {
                install_portable(artifact, candidate, target, root)?;
            }
        }
        if let Err(warning) = refresh_runtime_service() {
            warnings.push(warning);
        }
        Ok(InstallationOutcome {
            runtime_warning: (!warnings.is_empty()).then(|| warnings.join("; ")),
        })
    }
}

fn platform_architecture() -> Result<(&'static str, &'static str), String> {
    match std::env::consts::ARCH {
        "x86_64" => Ok(("amd64", "x86_64-unknown-linux-gnu")),
        "aarch64" => Ok(("arm64", "aarch64-unknown-linux-gnu")),
        architecture => Err(format!(
            "automatic updates are not available for {architecture}"
        )),
    }
}

#[cfg(test)]
fn portable_root(current_exe: &Path) -> Option<PathBuf> {
    let bin = current_exe.parent()?;
    if bin.file_name()? != OsStr::new("bin") || current_exe.file_name()? != OsStr::new("dogi") {
        return None;
    }
    let root = bin.parent()?;
    let desktop_file = root
        .join("share/applications")
        .join("io.github.oksyd.dogi.desktop");
    let metadata_file = root
        .join("share/metainfo")
        .join("io.github.oksyd.dogi.metainfo.xml");
    (root.join("LICENSE").is_file() && desktop_file.is_file() && metadata_file.is_file())
        .then(|| root.to_owned())
}

fn install_debian(
    artifact: &Path,
    candidate: &ReleaseCandidate,
    architecture: &str,
    current_exe: &Path,
) -> Result<(), InstallationError> {
    if !Path::new(PKEXEC).is_file() || !current_exe.is_file() {
        return Err(InstallationError::Failed(
            "the system package installer is unavailable".to_owned(),
        ));
    }
    let output = Command::new(PKEXEC)
        .arg(current_exe)
        .arg(INTERNAL_DEBIAN_INSTALL)
        .arg(artifact)
        .arg(candidate.artifact.size.to_string())
        .arg(&candidate.artifact.sha256)
        .arg(candidate.version.to_string())
        .arg(architecture)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| {
            InstallationError::Failed(format!(
                "could not start the system package installer: {error}"
            ))
        })?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(debian_install_error(output.status.code(), detail));
    }
    Ok(())
}

fn debian_install_error(code: Option<i32>, detail: String) -> InstallationError {
    match code {
        Some(126) => InstallationError::Cancelled,
        Some(127) => InstallationError::Authorization(
            "administrator authorization is unavailable".to_owned(),
        ),
        Some(code) if code == i32::from(HELPER_VERIFICATION_EXIT) => {
            InstallationError::Verification(nonempty_detail(
                detail,
                "the privileged installer rejected the release package",
            ))
        }
        Some(code) => InstallationError::Failed(nonempty_detail(
            detail,
            &format!("the system package installer exited with status {code}"),
        )),
        None => {
            InstallationError::Failed("the system package installer was interrupted".to_owned())
        }
    }
}

fn nonempty_detail(detail: String, fallback: &str) -> String {
    if detail.is_empty() {
        fallback.to_owned()
    } else {
        detail
    }
}

pub(super) fn run_internal_command() -> Option<ExitCode> {
    let mut arguments = std::env::args_os().skip(1);
    let command = arguments.next()?;
    let result = if command == OsStr::new(INTERNAL_DEBIAN_INSTALL) {
        run_privileged_debian_install(arguments.collect())
            .map_err(|error| (error.exit_code(), error.to_string()))
    } else if command == OsStr::new(INTERNAL_RELAUNCH) {
        run_relaunch(arguments.collect()).map_err(|detail| (HELPER_INSTALLATION_EXIT, detail))
    } else {
        return None;
    };
    Some(match result {
        Ok(()) => ExitCode::SUCCESS,
        Err((code, detail)) => {
            eprintln!("{detail}");
            ExitCode::from(code)
        }
    })
}

pub(super) fn schedule_relaunch_after_exit(current_exe: &Path) -> Result<(), String> {
    let mut command = Command::new(current_exe);
    command
        .arg(INTERNAL_RELAUNCH)
        .arg(std::process::id().to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command.spawn().map(|_| ()).map_err(|error| {
        format!("the update was installed but restart could not be scheduled: {error}")
    })
}

fn run_relaunch(arguments: Vec<OsString>) -> Result<(), String> {
    let [parent] = arguments.as_slice() else {
        return Err("invalid internal restart request".to_owned());
    };
    let parent = parent
        .to_str()
        .and_then(|value| value.parse::<libc::pid_t>().ok())
        .filter(|pid| *pid > 1)
        .ok_or_else(|| "invalid internal restart process id".to_owned())?;
    for _ in 0..600 {
        if !process_exists(parent) {
            let executable = std::env::current_exe().map_err(|error| {
                format!("could not locate the updated Dogi executable: {error}")
            })?;
            Command::new(executable)
                .arg("gui")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map(|_| ())
                .map_err(|error| format!("could not launch the updated Dogi: {error}"))?;
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err("the previous Dogi process did not exit within one minute".to_owned())
}

#[cfg(unix)]
fn process_exists(pid: libc::pid_t) -> bool {
    // SAFETY: `kill` with signal 0 performs only an existence/permission check and does not
    // deliver a signal. The PID was parsed as a positive `pid_t`.
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn process_exists(_pid: i32) -> bool {
    false
}

#[derive(Debug)]
enum PrivilegedInstallError {
    Verification(String),
    Installation(String),
}

impl PrivilegedInstallError {
    fn exit_code(&self) -> u8 {
        match self {
            Self::Verification(_) => HELPER_VERIFICATION_EXIT,
            Self::Installation(_) => HELPER_INSTALLATION_EXIT,
        }
    }
}

impl std::fmt::Display for PrivilegedInstallError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Verification(detail) | Self::Installation(detail) => formatter.write_str(detail),
        }
    }
}

fn run_privileged_debian_install(arguments: Vec<OsString>) -> Result<(), PrivilegedInstallError> {
    if !running_as_root() {
        return Err(PrivilegedInstallError::Installation(
            "the internal package installer requires administrator authorization".to_owned(),
        ));
    }
    if !Path::new(DPKG_DEB).is_file()
        || !Path::new(DPKG_QUERY).is_file()
        || !Path::new(APT_GET).is_file()
    {
        return Err(PrivilegedInstallError::Installation(
            "the Debian package installer is unavailable".to_owned(),
        ));
    }
    let [artifact, size, digest, version, architecture] = arguments.as_slice() else {
        return Err(PrivilegedInstallError::Verification(
            "invalid internal package installation request".to_owned(),
        ));
    };
    let version = version
        .to_str()
        .and_then(|value| Version::parse(value).ok())
        .ok_or_else(|| {
            PrivilegedInstallError::Verification(
                "the requested package version is invalid".to_owned(),
            )
        })?;
    let architecture = architecture.to_str().ok_or_else(|| {
        PrivilegedInstallError::Verification(
            "the requested package architecture is invalid".to_owned(),
        )
    })?;
    let expected_architecture = platform_architecture()
        .map_err(PrivilegedInstallError::Verification)?
        .0;
    if architecture != expected_architecture {
        return Err(PrivilegedInstallError::Verification(
            "the requested package architecture does not match this system".to_owned(),
        ));
    }
    let expected_name = format!("dogi_{version}_{architecture}.deb");
    let artifact = Path::new(artifact);
    if artifact.file_name() != Some(OsStr::new(&expected_name)) {
        return Err(PrivilegedInstallError::Verification(
            "the requested package name does not match the release".to_owned(),
        ));
    }

    let (expected_size, expected_sha256) = parse_install_integrity(size, digest)?;

    // The GUI verifies GitHub metadata and the signed tag before asking for authorization.
    // Pin the authorized bytes in root-owned staging so the download cannot change under apt.
    let installed = installed_dogi_version()?;
    if version <= installed {
        return Err(PrivilegedInstallError::Verification(format!(
            "Dogi {version} is not newer than the installed version {installed}"
        )));
    }
    let staged = stage_verified_package(artifact, &expected_name, expected_size, &expected_sha256)?;
    validate_debian_package(&staged.package, &version, architecture)
        .map_err(PrivilegedInstallError::Verification)?;
    let status = Command::new(APT_GET)
        .arg("--yes")
        .arg("--no-remove")
        .arg("install")
        .arg(&staged.package)
        .stdin(Stdio::null())
        .status()
        .map_err(|error| {
            PrivilegedInstallError::Installation(format!(
                "could not run the system package installer: {error}"
            ))
        })?;
    if !status.success() {
        return Err(PrivilegedInstallError::Installation(format!(
            "the system package installer exited with {}",
            status.code().map_or_else(
                || "an interruption".to_owned(),
                |code| format!("status {code}")
            )
        )));
    }
    let actual = installed_dogi_version()?;
    if actual != version {
        return Err(PrivilegedInstallError::Installation(format!(
            "the package installer completed but Dogi {actual} is installed instead of {version}"
        )));
    }
    Ok(())
}

fn installed_dogi_version() -> Result<Version, PrivilegedInstallError> {
    if !Path::new(DPKG_QUERY).is_file() {
        return Err(PrivilegedInstallError::Installation(
            "the Debian package database is unavailable".to_owned(),
        ));
    }
    let output = Command::new(DPKG_QUERY)
        .args(["--show", "--showformat=${Version}", "dogi"])
        .output()
        .map_err(|error| {
            PrivilegedInstallError::Installation(format!(
                "could not inspect the installed Dogi version: {error}"
            ))
        })?;
    if !output.status.success() {
        return Err(PrivilegedInstallError::Installation(
            "Dogi is not registered in the Debian package database".to_owned(),
        ));
    }
    let version = std::str::from_utf8(&output.stdout)
        .ok()
        .map(str::trim)
        .and_then(|value| Version::parse(value).ok())
        .ok_or_else(|| {
            PrivilegedInstallError::Installation(
                "the installed Dogi package has an unsupported version".to_owned(),
            )
        })?;
    Ok(version)
}

fn parse_install_integrity(
    size: &OsStr,
    digest: &OsStr,
) -> Result<(u64, String), PrivilegedInstallError> {
    let size = size
        .to_str()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|size| (1..=MAX_ARTIFACT_SIZE).contains(size))
        .ok_or_else(|| {
            PrivilegedInstallError::Verification("the requested package size is invalid".to_owned())
        })?;
    let digest = digest
        .to_str()
        .filter(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| {
            PrivilegedInstallError::Verification(
                "the requested package SHA-256 digest is invalid".to_owned(),
            )
        })?;
    Ok((size, digest.to_ascii_lowercase()))
}

struct StagedPackage {
    directory: PathBuf,
    package: PathBuf,
}

impl Drop for StagedPackage {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn stage_verified_package(
    source: &Path,
    name: &str,
    expected_size: u64,
    expected_sha256: &str,
) -> Result<StagedPackage, PrivilegedInstallError> {
    let directory = create_root_staging_directory()?;
    let package = directory.join(name);
    let result = copy_and_verify_package(source, &package, expected_size, expected_sha256);
    if let Err(error) = result {
        let _ = fs::remove_dir_all(&directory);
        return Err(error);
    }
    sync_directory(&directory);
    Ok(StagedPackage { directory, package })
}

fn create_root_staging_directory() -> Result<PathBuf, PrivilegedInstallError> {
    let parent = Path::new("/var/tmp");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for attempt in 0_u8..32 {
        let path = parent.join(format!(
            ".dogi-update-{}-{nonce}-{attempt}",
            std::process::id()
        ));
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        builder.mode(0o700);
        match builder.create(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(PrivilegedInstallError::Installation(format!(
                    "could not create the protected package staging directory: {error}"
                )));
            }
        }
    }
    Err(PrivilegedInstallError::Installation(
        "could not allocate a protected package staging directory".to_owned(),
    ))
}

fn copy_and_verify_package(
    source: &Path,
    destination: &Path,
    expected_size: u64,
    expected_sha256: &str,
) -> Result<(), PrivilegedInstallError> {
    let mut source_options = OpenOptions::new();
    source_options.read(true);
    #[cfg(unix)]
    source_options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let mut source = source_options.open(source).map_err(|error| {
        PrivilegedInstallError::Verification(format!(
            "could not open the downloaded package: {error}"
        ))
    })?;
    let metadata = source.metadata().map_err(|error| {
        PrivilegedInstallError::Verification(format!(
            "could not inspect the downloaded package: {error}"
        ))
    })?;
    if !metadata.is_file() || metadata.len() != expected_size {
        return Err(PrivilegedInstallError::Verification(
            "the downloaded package size does not match the selected release".to_owned(),
        ));
    }
    let mut destination_options = OpenOptions::new();
    destination_options.create_new(true).write(true);
    #[cfg(unix)]
    destination_options.mode(0o600);
    let mut destination = destination_options.open(destination).map_err(|error| {
        PrivilegedInstallError::Installation(format!(
            "could not create the protected package copy: {error}"
        ))
    })?;
    let mut hasher = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = source.read(&mut buffer).map_err(|error| {
            PrivilegedInstallError::Verification(format!(
                "could not read the downloaded package: {error}"
            ))
        })?;
        if count == 0 {
            break;
        }
        total = total.checked_add(count as u64).ok_or_else(|| {
            PrivilegedInstallError::Verification(
                "the downloaded package is larger than supported".to_owned(),
            )
        })?;
        if total > expected_size {
            return Err(PrivilegedInstallError::Verification(
                "the downloaded package changed while it was being verified".to_owned(),
            ));
        }
        hasher.update(&buffer[..count]);
        destination.write_all(&buffer[..count]).map_err(|error| {
            PrivilegedInstallError::Installation(format!(
                "could not write the protected package copy: {error}"
            ))
        })?;
    }
    if total != expected_size || lowercase_hex(&hasher.finalize()) != expected_sha256 {
        return Err(PrivilegedInstallError::Verification(
            "the downloaded package does not match the selected release".to_owned(),
        ));
    }
    destination.sync_all().map_err(|error| {
        PrivilegedInstallError::Installation(format!(
            "could not commit the protected package copy: {error}"
        ))
    })?;
    Ok(())
}

fn lowercase_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[(byte >> 4) as usize]));
        output.push(char::from(HEX[(byte & 0x0f) as usize]));
    }
    output
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

fn validate_debian_package(
    artifact: &Path,
    version: &Version,
    architecture: &str,
) -> Result<(), String> {
    let metadata = fs::symlink_metadata(artifact)
        .map_err(|error| format!("could not inspect the downloaded package: {error}"))?;
    if !metadata.file_type().is_file() {
        return Err("the downloaded package is not a regular file".to_owned());
    }
    for (field, expected) in [
        ("Package", "dogi".to_owned()),
        ("Version", version.to_string()),
        ("Architecture", architecture.to_owned()),
    ] {
        let output = Command::new(DPKG_DEB)
            .arg("--field")
            .arg(artifact)
            .arg(field)
            .output()
            .map_err(|error| format!("could not inspect the downloaded package: {error}"))?;
        if !output.status.success() {
            return Err("the downloaded Debian package is invalid".to_owned());
        }
        let actual = String::from_utf8(output.stdout)
            .map_err(|_| "the Debian package metadata is not UTF-8".to_owned())?;
        if actual.trim() != expected {
            return Err(format!("the Debian package has an unexpected {field}"));
        }
    }
    Ok(())
}

fn install_portable(
    artifact: &Path,
    candidate: &ReleaseCandidate,
    target: &str,
    current_root: &Path,
) -> Result<(), InstallationError> {
    let version = &candidate.version;
    let parent = current_root.parent().ok_or_else(|| {
        InstallationError::Failed("the portable installation has no parent directory".to_owned())
    })?;
    let staging =
        unique_sibling(parent, ".dogi-update-staging").map_err(InstallationError::Failed)?;
    fs::create_dir(&staging).map_err(|error| {
        InstallationError::Failed(format!("could not prepare the portable update: {error}"))
    })?;
    let mut cleanup = CleanupDirectory(Some(staging.clone()));

    let verified = stage_verified_portable(
        artifact,
        &staging,
        candidate.artifact.size,
        &candidate.artifact.sha256,
    )
    .map_err(InstallationError::Verification)?;
    install_verified_portable(verified, version, target, current_root, &staging)
        .map_err(InstallationError::Failed)?;
    cleanup.remove().map_err(|error| {
        InstallationError::Failed(format!("could not clean the portable transaction: {error}"))
    })?;
    sync_directory_checked(parent).map_err(|error| {
        InstallationError::Failed(format!(
            "could not persist portable transaction cleanup: {error}"
        ))
    })
}

fn install_verified_portable(
    verified: File,
    version: &Version,
    target: &str,
    current_root: &Path,
    staging: &Path,
) -> Result<(), String> {
    let parent = current_root
        .parent()
        .ok_or_else(|| "the portable installation has no parent directory".to_owned())?;
    let extracted = staging.join("root");
    fs::create_dir(&extracted)
        .map_err(|error| format!("could not prepare the portable update: {error}"))?;
    let result = extract_portable(verified, version, target, &extracted)
        .and_then(|()| validate_portable_root(&extracted));
    if let Err(error) = result {
        let _ = fs::remove_dir_all(staging);
        return Err(error);
    }
    sync_tree(&extracted).map_err(|error| {
        format!("could not persist the portable update before activation: {error}")
    })?;

    let root_name = current_root
        .file_name()
        .ok_or_else(|| "the portable installation root has no name".to_owned())?
        .to_string_lossy();
    let backup = parent.join(format!(".{root_name}.previous"));
    let retired = backup
        .exists()
        .then(|| unique_sibling(parent, ".dogi-update-retired"))
        .transpose()?;

    exchange_paths(current_root, &extracted)
        .map_err(|error| format!("could not atomically activate the portable update: {error}"))?;
    if let Err(error) =
        sync_directory_checked(parent).and_then(|()| sync_directory_checked(staging))
    {
        let rollback = exchange_paths(current_root, &extracted);
        return match rollback {
            Ok(()) => Err(format!(
                "could not persist the atomic portable activation: {error}"
            )),
            Err(rollback_error) => Err(format!(
                "could not persist the portable activation ({error}) or revert it ({rollback_error})"
            )),
        };
    }
    if let Some(retired) = &retired
        && let Err(error) = fs::rename(&backup, retired)
    {
        let rollback = exchange_paths(current_root, &extracted);
        return match rollback {
            Ok(()) => Err(format!(
                "could not rotate the previous portable version: {error}"
            )),
            Err(rollback_error) => Err(format!(
                "the update was activated, but its previous-version slot could not be rotated ({error}) and activation could not be reverted ({rollback_error})"
            )),
        };
    }
    if let Err(error) = fs::rename(&extracted, &backup) {
        let rollback = exchange_paths(current_root, &extracted);
        if let Some(retired) = &retired {
            let _ = fs::rename(retired, &backup);
        }
        return match rollback {
            Ok(()) => Err(format!(
                "could not preserve the previous portable version: {error}"
            )),
            Err(rollback_error) => Err(format!(
                "the update was activated, but its previous version could not be preserved ({error}) and activation could not be reverted ({rollback_error})"
            )),
        };
    }
    if let Err(error) =
        sync_directory_checked(parent).and_then(|()| sync_directory_checked(staging))
    {
        let restore_extracted = fs::rename(&backup, &extracted);
        let rollback = restore_extracted.and_then(|()| exchange_paths(current_root, &extracted));
        if let Some(retired) = &retired {
            let _ = fs::rename(retired, &backup);
        }
        return match rollback {
            Ok(()) => Err(format!(
                "could not persist the portable backup rotation: {error}"
            )),
            Err(rollback_error) => Err(format!(
                "could not persist the portable backup rotation ({error}) or revert activation ({rollback_error})"
            )),
        };
    }
    if let Some(retired) = retired {
        let _ = fs::remove_dir_all(retired);
    }
    sync_directory_checked(parent)
        .map_err(|error| format!("could not persist portable transaction cleanup: {error}"))?;
    Ok(())
}

struct CleanupDirectory(Option<PathBuf>);

impl CleanupDirectory {
    fn remove(&mut self) -> std::io::Result<()> {
        let Some(path) = self.0.take() else {
            return Ok(());
        };
        fs::remove_dir_all(path)
    }
}

impl Drop for CleanupDirectory {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = fs::remove_dir_all(path);
        }
    }
}

#[cfg(target_os = "linux")]
fn exchange_paths(left: &Path, right: &Path) -> std::io::Result<()> {
    let left = std::ffi::CString::new(left.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains NUL"))?;
    let right = std::ffi::CString::new(right.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains NUL"))?;
    // SAFETY: both C strings are NUL-terminated and remain alive for the syscall. `AT_FDCWD`
    // makes each absolute/relative path resolve exactly as the corresponding Rust path, and
    // `RENAME_EXCHANGE` atomically swaps two existing directory entries.
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            left.as_ptr(),
            libc::AT_FDCWD,
            right.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn stage_verified_portable(
    source: &Path,
    staging: &Path,
    expected_size: u64,
    expected_sha256: &str,
) -> Result<File, String> {
    let mut source_options = OpenOptions::new();
    source_options.read(true);
    #[cfg(unix)]
    source_options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let mut source = source_options
        .open(source)
        .map_err(|error| format!("could not securely open the portable update: {error}"))?;
    let metadata = source
        .metadata()
        .map_err(|error| format!("could not inspect the portable update: {error}"))?;
    if !metadata.is_file() || metadata.len() != expected_size {
        return Err("the portable update size does not match the selected release".to_owned());
    }

    let protected = staging.join("verified-archive");
    let mut options = OpenOptions::new();
    options.create_new(true).read(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut output = options
        .open(&protected)
        .map_err(|error| format!("could not create protected portable staging: {error}"))?;
    let mut hasher = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = source
            .read(&mut buffer)
            .map_err(|error| format!("could not read the portable update: {error}"))?;
        if count == 0 {
            break;
        }
        total = total
            .checked_add(count as u64)
            .ok_or_else(|| "the portable update is too large".to_owned())?;
        if total > expected_size {
            return Err("the portable update changed while it was being verified".to_owned());
        }
        hasher.update(&buffer[..count]);
        output
            .write_all(&buffer[..count])
            .map_err(|error| format!("could not stage the portable update: {error}"))?;
    }
    if total != expected_size || lowercase_hex(&hasher.finalize()) != expected_sha256 {
        return Err("the portable update does not match the selected release".to_owned());
    }
    output
        .sync_all()
        .and_then(|()| output.seek(SeekFrom::Start(0)).map(|_| ()))
        .map_err(|error| format!("could not commit protected portable staging: {error}"))?;
    fs::remove_file(protected)
        .map_err(|error| format!("could not seal protected portable staging: {error}"))?;
    Ok(output)
}

fn extract_portable(
    artifact: File,
    version: &Version,
    target: &str,
    destination: &Path,
) -> Result<(), String> {
    let decoder = zstd::Decoder::new(artifact)
        .map_err(|error| format!("could not decompress the portable update: {error}"))?;
    let mut archive = tar::Archive::new(decoder);
    let archive_root = PathBuf::from(format!("dogi-{version}-{target}"));
    let mut paths = HashSet::new();
    let mut entries = 0_usize;
    let mut unpacked_size = 0_u64;

    for entry in archive
        .entries()
        .map_err(|error| format!("could not read the portable update: {error}"))?
    {
        let mut entry =
            entry.map_err(|error| format!("could not read the portable update entry: {error}"))?;
        entries += 1;
        if entries > MAX_ARCHIVE_ENTRIES {
            return Err("the portable update contains too many files".to_owned());
        }
        unpacked_size = unpacked_size
            .checked_add(entry.size())
            .ok_or_else(|| "the portable update is too large".to_owned())?;
        if unpacked_size > MAX_UNPACKED_SIZE {
            return Err("the portable update expands beyond the safety limit".to_owned());
        }
        let kind = entry.header().entry_type();
        if !kind.is_file() && !kind.is_dir() {
            return Err("the portable update contains an unsupported entry type".to_owned());
        }
        let path = entry
            .path()
            .map_err(|error| format!("the portable update contains an invalid path: {error}"))?;
        let relative = path.strip_prefix(&archive_root).map_err(|_| {
            "the portable update contains a file outside its package root".to_owned()
        })?;
        if relative.as_os_str().is_empty() {
            continue;
        }
        if relative.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        }) {
            return Err("the portable update contains an unsafe path".to_owned());
        }
        if !paths.insert(relative.to_owned()) {
            return Err("the portable update contains duplicate paths".to_owned());
        }
        let output = destination.join(relative);
        if kind.is_dir() {
            fs::create_dir_all(&output).map_err(|error| {
                format!("could not create a portable update directory: {error}")
            })?;
        } else {
            let parent = output
                .parent()
                .ok_or_else(|| "the portable update contains an invalid file path".to_owned())?;
            fs::create_dir_all(parent).map_err(|error| {
                format!("could not create a portable update directory: {error}")
            })?;
            entry
                .unpack(&output)
                .map_err(|error| format!("could not extract the portable update: {error}"))?;
        }
    }
    Ok(())
}

fn validate_portable_root(root: &Path) -> Result<(), String> {
    let binary = root.join("bin/dogi");
    let metadata = fs::metadata(&binary)
        .map_err(|error| format!("the portable update has no Dogi executable: {error}"))?;
    if !metadata.is_file()
        || !root
            .join("share/applications/io.github.oksyd.dogi.desktop")
            .is_file()
        || !root
            .join("share/metainfo/io.github.oksyd.dogi.metainfo.xml")
            .is_file()
        || !root.join("LICENSE").is_file()
    {
        return Err("the portable update package is incomplete".to_owned());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err("the portable update executable is not executable".to_owned());
        }
    }
    Ok(())
}

fn unique_sibling(parent: &Path, prefix: &str) -> Result<PathBuf, String> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for attempt in 0_u8..16 {
        let path = parent.join(format!("{prefix}-{}-{nonce}-{attempt}", std::process::id()));
        if !path.exists() {
            return Ok(path);
        }
    }
    Err("could not allocate a unique update transaction path".to_owned())
}

fn refresh_runtime_service() -> Result<(), String> {
    if !Path::new(SYSTEMCTL).is_file() {
        return Err(
            "Dogi was updated, but systemd is unavailable; restart the background service manually"
                .to_owned(),
        );
    }
    run_user_systemctl(
        &["daemon-reload"],
        "reload the updated background service definition",
    )?;
    run_user_systemctl(
        &["try-restart", "dogi-runtime.service"],
        "restart the Dogi background service",
    )?;
    let status = Command::new(SYSTEMCTL)
        .args(["--user", "is-failed", "--quiet", "dogi-runtime.service"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|error| {
            format!(
                "Dogi was updated, but the background service health could not be checked: {error}"
            )
        })?;
    if status.success() {
        Err("Dogi was updated, but the background service failed after it was restarted".to_owned())
    } else {
        Ok(())
    }
}

fn run_user_systemctl(arguments: &[&str], action: &str) -> Result<(), String> {
    let output = Command::new(SYSTEMCTL)
        .arg("--user")
        .args(arguments)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("Dogi was updated, but it could not {action}: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    Err(if detail.is_empty() {
        format!("Dogi was updated, but it could not {action}")
    } else {
        format!("Dogi was updated, but it could not {action}: {detail}")
    })
}

fn sync_directory(path: &Path) {
    if let Ok(directory) = File::open(path) {
        let _ = directory.sync_all();
    }
}

fn sync_directory_checked(path: &Path) -> std::io::Result<()> {
    File::open(path)?.sync_all()
}

fn sync_tree(path: &Path) -> std::io::Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            sync_tree(&entry.path())?;
        } else if file_type.is_file() {
            File::open(entry.path())?.sync_all()?;
        }
    }
    File::open(path)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_TARGET: &str = "x86_64-unknown-linux-gnu";

    #[test]
    fn detects_a_complete_portable_layout() {
        let root = unique_test_root("portable-detect");
        fs::create_dir_all(root.join("bin")).unwrap();
        fs::create_dir_all(root.join("share/applications")).unwrap();
        fs::create_dir_all(root.join("share/metainfo")).unwrap();
        fs::write(root.join("bin/dogi"), b"binary").unwrap();
        fs::write(root.join("LICENSE"), b"license").unwrap();
        fs::write(
            root.join("share/applications/io.github.oksyd.dogi.desktop"),
            b"desktop",
        )
        .unwrap();
        fs::write(
            root.join("share/metainfo/io.github.oksyd.dogi.metainfo.xml"),
            b"metadata",
        )
        .unwrap();

        assert_eq!(portable_root(&root.join("bin/dogi")), Some(root.clone()));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn arbitrary_release_binary_is_not_treated_as_portable() {
        let root = unique_test_root("standalone");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("dogi"), b"binary").unwrap();

        assert_eq!(portable_root(&root.join("dogi")), None);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn platform_asset_names_match_release_packaging() {
        let version = Version::new(1, 2, 3);
        assert_eq!(
            ReleaseAssetKind::Debian {
                architecture: "amd64"
            }
            .expected_name(&version),
            "dogi_1.2.3_amd64.deb"
        );
        assert_eq!(
            ReleaseAssetKind::Portable {
                target: "x86_64-unknown-linux-gnu"
            }
            .expected_name(&version),
            "dogi-1.2.3-x86_64-unknown-linux-gnu.tar.zst"
        );
    }

    #[test]
    fn dismissed_authorization_is_a_neutral_cancellation() {
        assert_eq!(
            debian_install_error(Some(126), String::new()),
            InstallationError::Cancelled
        );
        assert!(matches!(
            debian_install_error(Some(127), String::new()),
            InstallationError::Authorization(_)
        ));
    }

    #[test]
    fn installation_integrity_requires_bounded_size_and_sha256() {
        let digest = "A".repeat(64);
        assert_eq!(
            parse_install_integrity(OsStr::new("1024"), OsStr::new(&digest)).unwrap(),
            (1024, digest.to_ascii_lowercase())
        );
        for size in [
            "0".to_owned(),
            "-1".to_owned(),
            (MAX_ARTIFACT_SIZE + 1).to_string(),
        ] {
            assert!(parse_install_integrity(OsStr::new(&size), OsStr::new(&digest)).is_err());
        }
        for digest in ["", "abc", &"g".repeat(64)] {
            assert!(parse_install_integrity(OsStr::new("1024"), OsStr::new(digest)).is_err());
        }
    }

    #[test]
    fn privileged_staging_copy_requires_the_expected_size_and_digest() {
        let base = unique_test_root("verified-staging");
        fs::create_dir_all(&base).unwrap();
        let source = base.join("download.deb");
        let destination = base.join("staged.deb");
        fs::write(&source, b"downloaded package bytes").unwrap();
        let digest = lowercase_hex(&Sha256::digest(b"downloaded package bytes"));

        copy_and_verify_package(
            &source,
            &destination,
            b"downloaded package bytes".len() as u64,
            &digest,
        )
        .unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"downloaded package bytes");

        let rejected = base.join("rejected.deb");
        assert!(
            copy_and_verify_package(
                &source,
                &rejected,
                b"downloaded package bytes".len() as u64,
                &"0".repeat(64),
            )
            .is_err()
        );
        fs::write(&source, b"changed after verification").unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"downloaded package bytes");
        assert!(
            copy_and_verify_package(&source, &base.join("wrong-size.deb"), 1, &digest).is_err()
        );
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn portable_installation_rechecks_the_download_before_replacing_the_application() {
        let base = unique_test_root("portable-reverification");
        let current = base.join("dogi-current");
        let version = Version::new(1, 2, 3);
        let name = format!("dogi-{version}-{TEST_TARGET}.tar.zst");
        let artifact = base.join(&name);
        fs::create_dir_all(current.join("bin")).unwrap();
        fs::write(current.join("bin/dogi"), b"old").unwrap();
        create_portable_archive(&artifact, &version, b"new");
        let bytes = fs::read(&artifact).unwrap();
        let candidate = ReleaseCandidate {
            version,
            artifact: super::super::github::ReleaseArtifact {
                name,
                url: String::new(),
                size: bytes.len() as u64,
                sha256: lowercase_hex(&Sha256::digest(&bytes)),
            },
        };

        let mut tampered = bytes.clone();
        tampered[0] ^= 1;
        fs::write(&artifact, tampered).unwrap();
        assert!(matches!(
            install_portable(&artifact, &candidate, TEST_TARGET, &current),
            Err(InstallationError::Verification(_))
        ));
        assert_eq!(fs::read(current.join("bin/dogi")).unwrap(), b"old");

        fs::write(&artifact, bytes).unwrap();
        install_portable(&artifact, &candidate, TEST_TARGET, &current).unwrap();
        assert_eq!(fs::read(current.join("bin/dogi")).unwrap(), b"new");
        assert_eq!(
            fs::read(base.join(".dogi-current.previous/bin/dogi")).unwrap(),
            b"old"
        );
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn portable_installation_retains_exactly_one_previous_tree() {
        let base = unique_test_root("portable-transaction");
        let current = base.join("dogi-current");
        let artifact = base.join("update.tar.zst");
        fs::create_dir_all(current.join("bin")).unwrap();
        fs::write(current.join("bin/dogi"), b"old").unwrap();
        create_portable_archive(&artifact, &Version::new(1, 2, 3), b"new");

        let staging = base.join("staging-1");
        fs::create_dir(&staging).unwrap();
        install_verified_portable(
            File::open(&artifact).unwrap(),
            &Version::new(1, 2, 3),
            TEST_TARGET,
            &current,
            &staging,
        )
        .unwrap();
        let backup = base.join(".dogi-current.previous");
        assert_eq!(fs::read(current.join("bin/dogi")).unwrap(), b"new");
        assert_eq!(fs::read(backup.join("bin/dogi")).unwrap(), b"old");

        create_portable_archive(&artifact, &Version::new(1, 2, 4), b"newer");
        let staging = base.join("staging-2");
        fs::create_dir(&staging).unwrap();
        install_verified_portable(
            File::open(&artifact).unwrap(),
            &Version::new(1, 2, 4),
            TEST_TARGET,
            &current,
            &staging,
        )
        .unwrap();
        assert_eq!(fs::read(current.join("bin/dogi")).unwrap(), b"newer");
        assert_eq!(fs::read(backup.join("bin/dogi")).unwrap(), b"new");
        assert_eq!(
            fs::read_dir(&base)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().contains("retired"))
                .count(),
            0
        );
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn portable_entrypoint_exchange_never_removes_the_live_path() {
        let base = unique_test_root("portable-exchange");
        let current = base.join("current");
        let replacement = base.join("replacement");
        fs::create_dir_all(&current).unwrap();
        fs::create_dir_all(&replacement).unwrap();
        fs::write(current.join("version"), b"old").unwrap();
        fs::write(replacement.join("version"), b"new").unwrap();

        assert!(current.is_dir());
        exchange_paths(&current, &replacement).unwrap();
        assert!(current.is_dir());
        assert_eq!(fs::read(current.join("version")).unwrap(), b"new");
        assert_eq!(fs::read(replacement.join("version")).unwrap(), b"old");

        exchange_paths(&current, &replacement).unwrap();
        assert!(current.is_dir());
        assert_eq!(fs::read(current.join("version")).unwrap(), b"old");
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn portable_extraction_rejects_links() {
        let base = unique_test_root("portable-link");
        let artifact = base.join("update.tar.zst");
        let destination = base.join("output");
        fs::create_dir_all(&destination).unwrap();
        let encoder = zstd::Encoder::new(File::create(&artifact).unwrap(), 0).unwrap();
        let mut archive = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_mode(0o777);
        header.set_size(0);
        header.set_cksum();
        archive
            .append_link(
                &mut header,
                "dogi-1.2.3-x86_64-unknown-linux-gnu/bin/dogi",
                "/usr/bin/dogi",
            )
            .unwrap();
        archive.into_inner().unwrap().finish().unwrap();

        let error = extract_portable(
            File::open(&artifact).unwrap(),
            &Version::new(1, 2, 3),
            TEST_TARGET,
            &destination,
        )
        .unwrap_err();
        assert!(error.contains("unsupported entry type"));
        let _ = fs::remove_dir_all(base);
    }

    fn create_portable_archive(path: &Path, version: &Version, binary: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let encoder = zstd::Encoder::new(File::create(path).unwrap(), 0).unwrap();
        let mut archive = tar::Builder::new(encoder);
        let root = format!("dogi-{version}-{TEST_TARGET}");
        for (relative, contents, mode) in [
            ("bin/dogi", binary, 0o755),
            ("LICENSE", b"license".as_slice(), 0o644),
            (
                "share/applications/io.github.oksyd.dogi.desktop",
                b"desktop".as_slice(),
                0o644,
            ),
            (
                "share/metainfo/io.github.oksyd.dogi.metainfo.xml",
                b"metadata".as_slice(),
                0o644,
            ),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_mode(mode);
            header.set_size(contents.len() as u64);
            header.set_cksum();
            archive
                .append_data(&mut header, format!("{root}/{relative}"), contents)
                .unwrap();
        }
        archive.into_inner().unwrap().finish().unwrap();
    }

    fn unique_test_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "dogi-update-{label}-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("unnamed")
        ))
    }
}

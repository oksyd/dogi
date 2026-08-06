use std::collections::HashSet;
#[cfg(test)]
use std::ffi::OsStr;
use std::fs::{self, File};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use semver::Version;

use super::github::ReleaseAssetKind;
use crate::environment::{AppEnvironment, Distribution};

const DPKG_DEB: &str = "/usr/bin/dpkg-deb";
const PKEXEC: &str = "/usr/bin/pkexec";
const APT_GET: &str = "/usr/bin/apt-get";
const SYSTEMCTL: &str = "/usr/bin/systemctl";
const MAX_ARCHIVE_ENTRIES: usize = 256;
const MAX_UNPACKED_SIZE: u64 = 256 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum InstallationError {
    Cancelled,
    Failed(String),
}

impl From<String> for InstallationError {
    fn from(detail: String) -> Self {
        Self::Failed(detail)
    }
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

    pub(super) fn install_and_restart(
        &self,
        artifact: &Path,
        version: &Version,
        current_exe: &Path,
    ) -> Result<(), InstallationError> {
        let backup = match self {
            Self::Debian { architecture } => {
                install_debian(artifact, version, architecture)?;
                None
            }
            Self::Portable { root, target } => {
                Some(install_portable(artifact, version, target, root)?)
            }
        };

        restart_runtime_service();
        let mut restart = Command::new(current_exe);
        restart
            .arg("gui")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Self::Portable { root, .. } = self {
            restart.current_dir(root);
        }
        let mut child = match restart.spawn() {
            Ok(child) => child,
            Err(error) => {
                if let (Some(backup), Self::Portable { root, .. }) = (&backup, self) {
                    rollback_portable(root, backup);
                    restart_runtime_service();
                }
                return Err(InstallationError::Failed(format!(
                    "the update was installed but Dogi could not restart: {error}"
                )));
            }
        };
        thread::sleep(Duration::from_secs(2));
        match child.try_wait() {
            Ok(None) => {}
            Ok(Some(status)) => {
                if let (Some(backup), Self::Portable { root, .. }) = (&backup, self) {
                    rollback_portable(root, backup);
                    restart_runtime_service();
                }
                return Err(InstallationError::Failed(format!(
                    "the updated Dogi process exited during its startup check ({status})"
                )));
            }
            Err(error) => {
                if let (Some(backup), Self::Portable { root, .. }) = (&backup, self) {
                    rollback_portable(root, backup);
                    restart_runtime_service();
                }
                return Err(InstallationError::Failed(format!(
                    "the updated Dogi process could not be checked: {error}"
                )));
            }
        }
        if let Some(backup) = backup {
            let _ = fs::remove_dir_all(backup);
        }
        Ok(())
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
    version: &Version,
    architecture: &str,
) -> Result<(), InstallationError> {
    if !Path::new(DPKG_DEB).is_file()
        || !Path::new(PKEXEC).is_file()
        || !Path::new(APT_GET).is_file()
    {
        return Err(InstallationError::Failed(
            "the system package installer is unavailable".to_owned(),
        ));
    }
    validate_debian_package(artifact, version, architecture)?;
    let status = Command::new(PKEXEC)
        .arg(APT_GET)
        .arg("--yes")
        .arg("--no-remove")
        .arg("install")
        .arg(artifact)
        .stdin(Stdio::null())
        .status()
        .map_err(|error| {
            InstallationError::Failed(format!(
                "could not start the system package installer: {error}"
            ))
        })?;
    if !status.success() {
        return Err(debian_install_error(status.code()));
    }
    Ok(())
}

fn debian_install_error(code: Option<i32>) -> InstallationError {
    match code {
        Some(126) => InstallationError::Cancelled,
        Some(127) => {
            InstallationError::Failed("administrator authorization is unavailable".to_owned())
        }
        Some(code) => InstallationError::Failed(format!(
            "the system package installer exited with status {code}"
        )),
        None => {
            InstallationError::Failed("the system package installer was interrupted".to_owned())
        }
    }
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
    version: &Version,
    target: &str,
    current_root: &Path,
) -> Result<PathBuf, String> {
    let parent = current_root
        .parent()
        .ok_or_else(|| "the portable installation has no parent directory".to_owned())?;
    let staging = unique_sibling(parent, ".dogi-update-staging")?;
    let extracted = staging.join("root");
    fs::create_dir(&staging)
        .and_then(|()| fs::create_dir(&extracted))
        .map_err(|error| format!("could not prepare the portable update: {error}"))?;

    let result = extract_portable(artifact, version, target, &extracted)
        .and_then(|()| validate_portable_root(&extracted));
    if let Err(error) = result {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }

    let backup = unique_sibling(parent, ".dogi-update-backup")?;
    if let Err(error) = fs::rename(current_root, &backup) {
        let _ = fs::remove_dir_all(&staging);
        return Err(format!(
            "could not preserve the current portable version: {error}"
        ));
    }
    if let Err(error) = fs::rename(&extracted, current_root) {
        let rollback = fs::rename(&backup, current_root);
        let _ = fs::remove_dir_all(&staging);
        return match rollback {
            Ok(()) => Err(format!("could not activate the portable update: {error}")),
            Err(rollback_error) => Err(format!(
                "could not activate the portable update ({error}) or restore it ({rollback_error})"
            )),
        };
    }
    let _ = fs::remove_dir(&staging);
    sync_directory(parent);
    Ok(backup)
}

fn extract_portable(
    artifact: &Path,
    version: &Version,
    target: &str,
    destination: &Path,
) -> Result<(), String> {
    let file = File::open(artifact)
        .map_err(|error| format!("could not open the portable update: {error}"))?;
    let decoder = zstd::Decoder::new(file)
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

fn rollback_portable(current_root: &Path, backup: &Path) {
    let Some(parent) = current_root.parent() else {
        return;
    };
    let failed = unique_sibling(parent, ".dogi-update-failed").ok();
    if let Some(failed) = &failed {
        let _ = fs::rename(current_root, failed);
    }
    if fs::rename(backup, current_root).is_ok()
        && let Some(failed) = failed
    {
        let _ = fs::remove_dir_all(failed);
    }
}

fn restart_runtime_service() {
    if Path::new(SYSTEMCTL).is_file() {
        let _ = Command::new(SYSTEMCTL)
            .args(["--user", "try-restart", "dogi-runtime.service"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn sync_directory(path: &Path) {
    if let Ok(directory) = File::open(path) {
        let _ = directory.sync_all();
    }
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
            debian_install_error(Some(126)),
            InstallationError::Cancelled
        );
        assert!(matches!(
            debian_install_error(Some(127)),
            InstallationError::Failed(_)
        ));
    }

    #[test]
    fn portable_installation_can_be_activated_and_rolled_back() {
        let base = unique_test_root("portable-transaction");
        let current = base.join("dogi-current");
        let artifact = base.join("update.tar.zst");
        fs::create_dir_all(current.join("bin")).unwrap();
        fs::write(current.join("bin/dogi"), b"old").unwrap();
        create_portable_archive(&artifact, &Version::new(1, 2, 3), b"new");

        let backup =
            install_portable(&artifact, &Version::new(1, 2, 3), TEST_TARGET, &current).unwrap();
        assert_eq!(fs::read(current.join("bin/dogi")).unwrap(), b"new");
        assert_eq!(fs::read(backup.join("bin/dogi")).unwrap(), b"old");

        rollback_portable(&current, &backup);
        assert_eq!(fs::read(current.join("bin/dogi")).unwrap(), b"old");
        assert!(!backup.exists());
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

        let error = extract_portable(&artifact, &Version::new(1, 2, 3), TEST_TARGET, &destination)
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

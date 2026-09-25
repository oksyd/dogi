#[cfg(target_os = "linux")]
use std::ffi::CString;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(target_os = "linux")]
use std::os::fd::FromRawFd;
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(all(unix, any(test, not(target_os = "linux"))))]
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static TEMPORARY_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const FILE_LOCK_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FileOwner {
    uid: u32,
    gid: u32,
}

impl FileOwner {
    pub(crate) const fn new(uid: u32, gid: u32) -> Self {
        Self { uid, gid }
    }
}

#[derive(Debug)]
pub(crate) struct PersistenceError {
    operation: &'static str,
    path: PathBuf,
    source: io::Error,
}

impl PersistenceError {
    fn new(operation: &'static str, path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self {
            operation,
            path: path.into(),
            source,
        }
    }
}

impl fmt::Display for PersistenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "could not {} {}: {}",
            self.operation,
            self.path.display(),
            self.source
        )
    }
}

impl std::error::Error for PersistenceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

pub(crate) struct ExclusiveFileLock {
    file: File,
}

impl ExclusiveFileLock {
    pub(crate) fn acquire(path: &Path, owner: Option<FileOwner>) -> Result<Self, PersistenceError> {
        Self::acquire_with_timeout(path, owner, FILE_LOCK_TIMEOUT)
    }

    fn acquire_with_timeout(
        path: &Path,
        owner: Option<FileOwner>,
        timeout: Duration,
    ) -> Result<Self, PersistenceError> {
        let parent = parent_directory(path)?;
        ensure_private_directory(parent, owner)?;

        let file = secure_open_options(true)
            .read(true)
            .write(true)
            .open(path)
            .map_err(|source| PersistenceError::new("open lock", path, source))?;
        set_owner(&file, owner, path)?;
        lock_exclusive(&file, timeout)
            .map_err(|source| PersistenceError::new("lock", path, source))?;
        Ok(Self { file })
    }
}

impl Drop for ExclusiveFileLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            // SAFETY: `self.file` owns a live file descriptor for the whole call. Unlocking an
            // already-unlocked descriptor is harmless, and Drop cannot report an I/O failure.
            let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

pub(crate) fn sibling_lock_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("data");
    path.with_file_name(format!(".{file_name}.lock"))
}

pub(crate) fn atomic_write(
    path: &Path,
    bytes: &[u8],
    owner: Option<FileOwner>,
) -> Result<(), PersistenceError> {
    let parent = parent_directory(path)?;
    ensure_private_directory(parent, owner)?;
    let (temporary_path, mut temporary) = create_temporary_file(path, owner)?;

    let write_result = (|| {
        temporary
            .write_all(bytes)
            .map_err(|source| PersistenceError::new("write", &temporary_path, source))?;
        temporary
            .sync_all()
            .map_err(|source| PersistenceError::new("sync", &temporary_path, source))?;
        drop(temporary);
        fs::rename(&temporary_path, path)
            .map_err(|source| PersistenceError::new("replace", path, source))?;
        sync_directory(parent)
    })();

    if write_result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    write_result
}

pub(crate) fn quarantine(path: &Path, label: &str) -> Result<Option<PathBuf>, PersistenceError> {
    match fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(PersistenceError::new("inspect", path, source)),
    }
    let parent = parent_directory(path)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("data");
    let label = label
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || *character == '-')
        .collect::<String>();
    let label = if label.is_empty() { "invalid" } else { &label };
    let epoch_seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    for _ in 0..128 {
        let sequence = TEMPORARY_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let destination = parent.join(format!(
            "{file_name}.{label}-{epoch_seconds}-{sequence:x}.bak"
        ));
        match rename_without_replacement(path, &destination) {
            Ok(()) => {
                sync_directory(parent)?;
                return Ok(Some(destination));
            }
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(PersistenceError::new("quarantine", path, source));
            }
        }
    }

    Err(PersistenceError::new(
        "quarantine",
        path,
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            "quarantine filename namespace is exhausted",
        ),
    ))
}

pub(crate) fn durable_remove(path: &Path) -> Result<(), PersistenceError> {
    match fs::remove_file(path) {
        Ok(()) => sync_directory(parent_directory(path)?),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(PersistenceError::new("remove", path, source)),
    }
}

pub(crate) fn ensure_private_directory(
    path: &Path,
    owner: Option<FileOwner>,
) -> Result<(), PersistenceError> {
    ensure_private_directory_platform(path, owner)
}

#[cfg(target_os = "linux")]
fn ensure_private_directory_platform(
    path: &Path,
    owner: Option<FileOwner>,
) -> Result<(), PersistenceError> {
    let components = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(Ok(name)),
            Component::RootDir | Component::CurDir => None,
            Component::ParentDir | Component::Prefix(_) => Some(Err(PersistenceError::new(
                "resolve directory",
                path,
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "private directory path contains an unsafe component",
                ),
            ))),
        })
        .collect::<Result<Vec<_>, _>>()?;
    if components.is_empty() {
        return Err(PersistenceError::new(
            "resolve directory",
            path,
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "private directory path does not name a directory below the filesystem root",
            ),
        ));
    }

    let start = if path.is_absolute() { "/" } else { "." };
    let mut directory = File::open(start)
        .map_err(|source| PersistenceError::new("open directory", start, source))?;
    for component in components {
        let component = CString::new(component.as_bytes()).map_err(|_| {
            PersistenceError::new(
                "resolve directory",
                path,
                io::Error::new(io::ErrorKind::InvalidInput, "directory name contains NUL"),
            )
        })?;
        // SAFETY: `directory` owns a live directory descriptor and `component` is a
        // NUL-terminated single path component. mkdirat does not retain either argument.
        let created = unsafe { libc::mkdirat(directory.as_raw_fd(), component.as_ptr(), 0o700) };
        let created = if created == 0 {
            true
        } else {
            let source = io::Error::last_os_error();
            if source.kind() == io::ErrorKind::AlreadyExists {
                false
            } else {
                return Err(PersistenceError::new("create directory", path, source));
            }
        };
        // SAFETY: the parent fd and C string remain valid for the call. O_NOFOLLOW and
        // O_DIRECTORY ensure a symlink or non-directory component is rejected atomically.
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                component.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(PersistenceError::new(
                "open directory without following symlinks",
                path,
                io::Error::last_os_error(),
            ));
        }
        // SAFETY: openat returned a new owned descriptor and ownership is transferred to File.
        let child = unsafe { File::from_raw_fd(fd) };
        if created {
            set_owner(&child, owner, path)?;
        }
        directory = child;
    }

    // SAFETY: `directory` is the final O_DIRECTORY descriptor opened with O_NOFOLLOW.
    if unsafe { libc::fchmod(directory.as_raw_fd(), 0o700) } != 0 {
        return Err(PersistenceError::new(
            "set directory permissions for",
            path,
            io::Error::last_os_error(),
        ));
    }
    set_owner(&directory, owner, path)
}

#[cfg(not(target_os = "linux"))]
fn ensure_private_directory_platform(
    path: &Path,
    owner: Option<FileOwner>,
) -> Result<(), PersistenceError> {
    fs::create_dir_all(path)
        .map_err(|source| PersistenceError::new("create directory", path, source))?;
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|source| PersistenceError::new("set directory permissions for", path, source))?;
    let directory =
        File::open(path).map_err(|source| PersistenceError::new("open directory", path, source))?;
    set_owner(&directory, owner, path)
}

fn parent_directory(path: &Path) -> Result<&Path, PersistenceError> {
    path.parent().ok_or_else(|| {
        PersistenceError::new(
            "resolve parent for",
            path,
            io::Error::new(io::ErrorKind::InvalidInput, "path has no parent directory"),
        )
    })
}

fn create_temporary_file(
    destination: &Path,
    owner: Option<FileOwner>,
) -> Result<(PathBuf, File), PersistenceError> {
    let parent = parent_directory(destination)?;
    let file_name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("data");
    let epoch_nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();

    for _ in 0..128 {
        let sequence = TEMPORARY_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(
            ".{file_name}.tmp-{}-{epoch_nanos:x}-{sequence:x}",
            std::process::id()
        ));
        match secure_open_options(false).write(true).open(&path) {
            Ok(file) => {
                set_owner(&file, owner, &path)?;
                return Ok((path, file));
            }
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(PersistenceError::new("create temporary file", path, source));
            }
        }
    }

    Err(PersistenceError::new(
        "create temporary file for",
        destination,
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            "temporary file namespace is exhausted",
        ),
    ))
}

fn secure_open_options(create_if_missing: bool) -> OpenOptions {
    let mut options = OpenOptions::new();
    if create_if_missing {
        options.create(true);
    } else {
        options.create_new(true);
    }
    #[cfg(unix)]
    {
        options.mode(0o600);
        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    options
}

#[cfg(unix)]
fn lock_exclusive(file: &File, timeout: Duration) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    let mut delay = Duration::from_millis(5);
    loop {
        // SAFETY: `file` owns a valid descriptor for this call. flock does not retain pointers
        // or access Rust memory; LOCK_NB makes contention observable and therefore bounded.
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result == 0 {
            return Ok(());
        }
        let source = io::Error::last_os_error();
        if source.kind() != io::ErrorKind::WouldBlock {
            return Err(source);
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "another Dogi process kept the file busy beyond the lock timeout",
            ));
        };
        std::thread::sleep(delay.min(remaining));
        delay = (delay * 2).min(Duration::from_millis(50));
    }
}

#[cfg(not(unix))]
fn lock_exclusive(_file: &File, _timeout: Duration) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "file locking requires Unix",
    ))
}

#[cfg(unix)]
fn set_owner(file: &File, owner: Option<FileOwner>, path: &Path) -> Result<(), PersistenceError> {
    let Some(owner) = owner else {
        return Ok(());
    };
    // SAFETY: `file` owns a valid descriptor, and uid/gid are plain values supplied by the
    // resolved desktop-user context. `fchown` neither retains pointers nor aliases Rust memory.
    let result = unsafe { libc::fchown(file.as_raw_fd(), owner.uid, owner.gid) };
    if result == 0 {
        Ok(())
    } else {
        Err(PersistenceError::new(
            "set ownership for",
            path,
            io::Error::last_os_error(),
        ))
    }
}

#[cfg(not(unix))]
fn set_owner(
    _file: &File,
    _owner: Option<FileOwner>,
    _path: &Path,
) -> Result<(), PersistenceError> {
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), PersistenceError> {
    let directory =
        File::open(path).map_err(|source| PersistenceError::new("open directory", path, source))?;
    directory
        .sync_all()
        .map_err(|source| PersistenceError::new("sync directory", path, source))
}

#[cfg(target_os = "linux")]
fn rename_without_replacement(source: &Path, destination: &Path) -> io::Result<()> {
    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "source path contains NUL"))?;
    let destination = CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "destination path contains NUL")
    })?;
    // SAFETY: both C strings are NUL-terminated and live for the duration of the call. Relative
    // paths are resolved against `AT_FDCWD`; `RENAME_NOREPLACE` prevents clobbering a backup.
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "linux"))]
fn rename_without_replacement(source: &Path, destination: &Path) -> io::Result<()> {
    fs::hard_link(source, destination)?;
    if let Err(error) = fs::remove_file(source) {
        let _ = fs::remove_file(destination);
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    fn test_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "dogi-persistence-{name}-{}-{}",
            std::process::id(),
            TEMPORARY_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn atomic_write_replaces_contents_and_leaves_no_temporary_file() {
        let root = test_path("atomic");
        let path = root.join("config.json");
        atomic_write(&path, b"first", None).unwrap();
        atomic_write(&path, b"second", None).unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"second");
        assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
        #[cfg(unix)]
        {
            assert_eq!(
                fs::metadata(&root).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn lock_path_is_stable_and_hidden() {
        assert_eq!(
            sibling_lock_path(Path::new("/tmp/config.json")),
            Path::new("/tmp/.config.json.lock")
        );
    }

    #[test]
    fn exclusive_lock_serializes_independent_callers() {
        let root = test_path("lock");
        let lock_path = root.join("data.lock");
        let first = ExclusiveFileLock::acquire(&lock_path, None).unwrap();
        let (acquired_sender, acquired_receiver) = mpsc::channel();
        let contender = std::thread::spawn(move || {
            let second = ExclusiveFileLock::acquire(&lock_path, None).unwrap();
            acquired_sender.send(()).unwrap();
            drop(second);
        });

        assert!(
            acquired_receiver
                .recv_timeout(Duration::from_millis(50))
                .is_err()
        );
        drop(first);
        acquired_receiver
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        contender.join().unwrap();
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn exclusive_lock_reports_contention_after_a_bounded_wait() {
        let root = test_path("lock-timeout");
        let lock_path = root.join("data.lock");
        let first = ExclusiveFileLock::acquire(&lock_path, None).unwrap();
        let started = Instant::now();

        let error =
            ExclusiveFileLock::acquire_with_timeout(&lock_path, None, Duration::from_millis(30))
                .err()
                .unwrap();

        assert!(error.to_string().contains("busy beyond the lock timeout"));
        assert!(started.elapsed() >= Duration::from_millis(20));
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(first);
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn private_directory_creation_rejects_symlink_components() {
        use std::os::unix::fs::symlink;

        let root = test_path("directory-symlink");
        let target = test_path("directory-symlink-target");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        symlink(&target, root.join("redirect")).unwrap();

        let error = atomic_write(&root.join("redirect/config.json"), b"unsafe", None)
            .err()
            .unwrap();

        assert!(error.to_string().contains("without following symlinks"));
        assert!(!target.join("config.json").exists());
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o755
        );
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(target);
    }

    #[cfg(unix)]
    #[test]
    fn lock_file_symlinks_are_rejected() {
        use std::os::unix::fs::symlink;

        let root = test_path("symlink");
        fs::create_dir_all(&root).unwrap();
        let target = root.join("target");
        fs::write(&target, b"untouched").unwrap();
        let lock_path = root.join("data.lock");
        symlink(&target, &lock_path).unwrap();

        assert!(ExclusiveFileLock::acquire(&lock_path, None).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"untouched");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn quarantine_preserves_contents_without_overwriting_an_existing_backup() {
        let root = test_path("quarantine");
        let path = root.join("config.json");
        atomic_write(&path, b"invalid config", None).unwrap();

        let backup = quarantine(&path, "unsupported-schema").unwrap().unwrap();

        assert!(!path.exists());
        assert_eq!(fs::read(&backup).unwrap(), b"invalid config");
        assert_eq!(quarantine(&path, "unsupported-schema").unwrap(), None);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn durable_remove_is_idempotent() {
        let root = test_path("remove");
        let path = root.join("journal.json");
        atomic_write(&path, b"transaction", None).unwrap();

        durable_remove(&path).unwrap();
        durable_remove(&path).unwrap();

        assert!(!path.exists());
        let _ = fs::remove_dir_all(root);
    }
}

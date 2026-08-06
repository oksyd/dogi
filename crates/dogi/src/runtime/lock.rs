use std::fs::{self, File, OpenOptions};
use std::path::Path;

use dogi_core::{DogiError, Result};

pub(crate) struct ProcessLock {
    file: File,
}

impl ProcessLock {
    pub(crate) fn acquire(path: &Path, purpose: &str) -> Result<Self> {
        let parent = path.parent().ok_or_else(|| {
            DogiError::Config(format!("lock path has no parent: {}", path.display()))
        })?;
        fs::create_dir_all(parent).map_err(|error| {
            DogiError::Config(format!("failed to create {}: {error}", parent.display()))
        })?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .map_err(|error| {
                DogiError::BackendUnavailable(format!(
                    "failed to open {purpose} lock {}: {error}",
                    path.display()
                ))
            })?;
        lock_exclusive(&file).map_err(|error| {
            DogiError::BackendUnavailable(format!(
                "another Dogi {purpose} is already active ({error})"
            ))
        })?;
        Ok(Self { file })
    }
}

#[cfg(unix)]
fn lock_exclusive(file: &File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(unix))]
fn lock_exclusive(_file: &File) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "process locks require Unix",
    ))
}

impl Drop for ProcessLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;

            let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_active_lock_cannot_be_acquired_twice() {
        let path = std::env::temp_dir().join(format!("dogi-lock-{}", std::process::id()));
        let first = ProcessLock::acquire(&path, "test").unwrap();
        assert!(ProcessLock::acquire(&path, "test").is_err());
        drop(first);
        assert!(ProcessLock::acquire(&path, "test").is_ok());
        let _ = fs::remove_file(path);
    }
}

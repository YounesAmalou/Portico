//! Files every system shares: locking, atomic-write bookkeeping, path normalisation and the
//! test hooks that inject failures.

use std::{
    fs::{self, File, OpenOptions},
    io,
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::Duration,
};

use fs2::FileExt;

#[cfg(unix)]
use std::fs::rename as replace;

#[cfg(windows)]
use super::super::windows::fs::replace;

pub fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

pub fn reject_symlink(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "managed path is a symbolic link",
        )),
        Ok(metadata) if !metadata.is_file() => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "managed path is not a file",
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

pub fn open_lock(path: &Path) -> io::Result<File> {
    reject_symlink(path)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(file) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                file.set_permissions(fs::Permissions::from_mode(0o600))?;
            }
            file.sync_all()?;
            Ok(file)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            OpenOptions::new().read(true).write(true).open(path)
        }
        Err(error) => Err(error),
    }
}

pub fn acquire_lock(file: &File, cancelled: Option<&AtomicBool>) -> io::Result<()> {
    let Some(cancelled) = cancelled else {
        return FileExt::lock_exclusive(file);
    };
    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "operation cancelled",
            ));
        }
        match FileExt::try_lock_exclusive(file) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(error),
        }
    }
}

/// Reads a managed config for mapping OpenSSH's line numbers back to hosts.
///
/// A read failure yields empty text: the line then maps to no host, so the rejection degrades
/// to an unmapped one, which is still reported verbatim. Nothing depends on the text otherwise.
pub fn read_lossy(path: &Path) -> String {
    fs::read(path)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default()
}

pub struct PendingFile {
    path: Option<PathBuf>,
}

impl PendingFile {
    /// Owns the file at `path`: it is removed when dropped, unless renamed or kept.
    pub(in crate::platform) fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    pub fn path(&self) -> &Path {
        self.path.as_deref().expect("pending file has a path")
    }

    /// Moves the file over `destination` in one step, replacing what is there.
    pub fn rename_to(mut self, destination: &Path) -> io::Result<()> {
        replace(self.path(), destination)?;
        self.path = None;
        Ok(())
    }

    /// Keeps the file instead of removing it when dropped, and returns its path.
    pub fn keep(mut self) -> PathBuf {
        self.path.take().expect("pending file has a path")
    }
}

impl Drop for PendingFile {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = fs::remove_file(path);
        }
    }
}

/// What tells one version of a path from another. An absent or unreadable path has none.
///
/// The inode catches a file renamed into place within one timestamp tick, and the status-change
/// time a copy that kept the old modification time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Stamp {
    pub device: u64,
    pub inode: u64,
    pub size: u64,
    pub modified: (i64, i64),
    pub changed: (i64, i64),
}

#[cfg(test)]
thread_local! {
    static FAIL_TRANSACTION_ROLLBACK_WRITE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub fn fail_next_transaction_rollback_write() {
    FAIL_TRANSACTION_ROLLBACK_WRITE.with(|fail| fail.set(true));
}

/// Whether the next rollback write must fail; the failure is spent by asking.
#[cfg(test)]
pub(in crate::platform) fn take_rollback_write_failure() -> bool {
    FAIL_TRANSACTION_ROLLBACK_WRITE.with(|fail| fail.replace(false))
}

#[cfg(test)]
thread_local! {
    static DIRECTORY_SYNC_FAILURES: std::cell::RefCell<std::collections::VecDeque<(PathBuf, usize)>> = const { std::cell::RefCell::new(std::collections::VecDeque::new()) };
}

#[cfg(test)]
pub fn fail_next_directory_sync(path: &Path) {
    fail_directory_sync_after(path, 0);
}

#[cfg(test)]
pub fn fail_directory_sync_after(path: &Path, successful_syncs: usize) {
    DIRECTORY_SYNC_FAILURES.with(|failures| {
        failures
            .borrow_mut()
            .push_back((path.to_owned(), successful_syncs))
    });
}

/// Whether the sync of the directory `path` must fail now, consuming one queued failure.
#[cfg(test)]
pub(in crate::platform) fn take_directory_sync_failure(path: &Path) -> bool {
    DIRECTORY_SYNC_FAILURES.with(|failures| {
        let mut failures = failures.borrow_mut();
        let Some((target, remaining)) = failures.front_mut() else {
            return false;
        };
        if target != path {
            return false;
        }
        if *remaining > 0 {
            *remaining -= 1;
            return false;
        }
        failures.pop_front();
        true
    })
}
